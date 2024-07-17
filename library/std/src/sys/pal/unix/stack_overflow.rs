#![cfg_attr(test, allow(dead_code))]

use self::imp::{drop_handler, make_handler};

pub use self::imp::cleanup;
pub use self::imp::init;

pub struct Handler {
    data: *mut libc::c_void,
}

impl Handler {
    pub unsafe fn new() -> Handler {
        make_handler(false)
    }

    fn null() -> Handler {
        Handler { data: crate::ptr::null_mut() }
    }
}

impl Drop for Handler {
    fn drop(&mut self) {
        unsafe {
            drop_handler(self.data);
        }
    }
}

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "hurd",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "solaris"
))]
mod imp {
    use super::Handler;
    use crate::cell::Cell;
    use crate::io;
    use crate::mem;
    use crate::ops::Range;
    use crate::ptr;
    use crate::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
    use crate::sys::pal::unix::os;
    use crate::thread;

    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    use libc::{mmap as mmap64, mprotect, munmap};
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    use libc::{mmap64, mprotect, munmap};
    use libc::{sigaction, sighandler_t, SA_ONSTACK, SA_SIGINFO, SIGBUS, SIGSEGV, SIG_DFL};
    use libc::{sigaltstack, SS_DISABLE};
    use libc::{MAP_ANON, MAP_FAILED, MAP_FIXED, MAP_PRIVATE, PROT_NONE, PROT_READ, PROT_WRITE};

    // We use a TLS variable to store the address of the guard page. While TLS
    // variables are not guaranteed to be signal-safe, this works out in practice
    // since we make sure to write to the variable before the signal stack is
    // installed, thereby ensuring that the variable is always allocated when
    // the signal handler is called.
    thread_local! {
        // FIXME: use `Range` once that implements `Copy`.
        static GUARD: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    }

    // Signal handler for the SIGSEGV and SIGBUS handlers. We've got guard pages
    // (unmapped pages) at the end of every thread's stack, so if a thread ends
    // up running into the guard page it'll trigger this handler. We want to
    // detect these cases and print out a helpful error saying that the stack
    // has overflowed. All other signals, however, should go back to what they
    // were originally supposed to do.
    //
    // This handler currently exists purely to print an informative message
    // whenever a thread overflows its stack. We then abort to exit and
    // indicate a crash, but to avoid a misleading SIGSEGV that might lead
    // users to believe that unsafe code has accessed an invalid pointer; the
    // SIGSEGV encountered when overflowing the stack is expected and
    // well-defined.
    //
    // If this is not a stack overflow, the handler un-registers itself and
    // then returns (to allow the original signal to be delivered again).
    // Returning from this kind of signal handler is technically not defined
    // to work when reading the POSIX spec strictly, but in practice it turns
    // out many large systems and all implementations allow returning from a
    // signal handler to work. For a more detailed explanation see the
    // comments on #26458.
    /// SIGSEGV/SIGBUS entry point
    /// # Safety
    /// Rust doesn't call this, it *gets called*.
    #[forbid(unsafe_op_in_unsafe_fn)]
    unsafe extern "C" fn signal_handler(
        signum: libc::c_int,
        info: *mut libc::siginfo_t,
        _data: *mut libc::c_void,
    ) {
        let (start, end) = GUARD.get();
        // SAFETY: this pointer is provided by the system and will always point to a valid `siginfo_t`.
        let addr = unsafe { (*info).si_addr().addr() };

        // If the faulting address is within the guard page, then we print a
        // message saying so and abort.
        if start <= addr && addr < end {
            rtprintpanic!(
                "\nthread '{}' has overflowed its stack\n",
                thread::current().name().unwrap_or("<unknown>")
            );
            rtabort!("stack overflow");
        } else {
            // Unregister ourselves by reverting back to the default behavior.
            // SAFETY: assuming all platforms define struct sigaction as "zero-initializable"
            let mut action: sigaction = unsafe { mem::zeroed() };
            action.sa_sigaction = SIG_DFL;
            // SAFETY: pray this is a well-behaved POSIX implementation of fn sigaction
            unsafe { sigaction(signum, &action, ptr::null_mut()) };

            // See comment above for why this function returns.
        }
    }

    static PAGE_SIZE: AtomicUsize = AtomicUsize::new(0);
    static MAIN_ALTSTACK: AtomicPtr<libc::c_void> = AtomicPtr::new(ptr::null_mut());
    static NEED_ALTSTACK: AtomicBool = AtomicBool::new(false);

    pub unsafe fn init() {
        PAGE_SIZE.store(os::page_size(), Ordering::Relaxed);

        // Always write to GUARD to ensure the TLS variable is allocated.
        let guard = install_main_guard().unwrap_or(0..0);
        GUARD.set((guard.start, guard.end));

        let mut action: sigaction = mem::zeroed();
        for &signal in &[SIGSEGV, SIGBUS] {
            sigaction(signal, ptr::null_mut(), &mut action);
            // Configure our signal handler if one is not already set.
            if action.sa_sigaction == SIG_DFL {
                action.sa_flags = SA_SIGINFO | SA_ONSTACK;
                action.sa_sigaction = signal_handler as sighandler_t;
                sigaction(signal, &action, ptr::null_mut());
                NEED_ALTSTACK.store(true, Ordering::Relaxed);
            }
        }

        let handler = make_handler(true);
        MAIN_ALTSTACK.store(handler.data, Ordering::Relaxed);
        mem::forget(handler);
    }

    pub unsafe fn cleanup() {
        drop_handler(MAIN_ALTSTACK.load(Ordering::Relaxed));
    }

    unsafe fn get_stack() -> libc::stack_t {
        // OpenBSD requires this flag for stack mapping
        // otherwise the said mapping will fail as a no-op on most systems
        // and has a different meaning on FreeBSD
        #[cfg(any(
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "linux",
            target_os = "dragonfly",
        ))]
        let flags = MAP_PRIVATE | MAP_ANON | libc::MAP_STACK;
        #[cfg(not(any(
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "linux",
            target_os = "dragonfly",
        )))]
        let flags = MAP_PRIVATE | MAP_ANON;

        let sigstack_size = sigstack_size();
        let page_size = PAGE_SIZE.load(Ordering::Relaxed);

        let stackp = mmap64(
            ptr::null_mut(),
            sigstack_size + page_size,
            PROT_READ | PROT_WRITE,
            flags,
            -1,
            0,
        );
        if stackp == MAP_FAILED {
            panic!("failed to allocate an alternative stack: {}", io::Error::last_os_error());
        }
        let guard_result = libc::mprotect(stackp, page_size, PROT_NONE);
        if guard_result != 0 {
            panic!("failed to set up alternative stack guard page: {}", io::Error::last_os_error());
        }
        let stackp = stackp.add(page_size);

        libc::stack_t { ss_sp: stackp, ss_flags: 0, ss_size: sigstack_size }
    }

    pub unsafe fn make_handler(main_thread: bool) -> Handler {
        if !NEED_ALTSTACK.load(Ordering::Relaxed) {
            return Handler::null();
        }

        if !main_thread {
            // Always write to GUARD to ensure the TLS variable is allocated.
            let guard = current_guard().unwrap_or(0..0);
            GUARD.set((guard.start, guard.end));
        }

        let mut stack = mem::zeroed();
        sigaltstack(ptr::null(), &mut stack);
        // Configure alternate signal stack, if one is not already set.
        if stack.ss_flags & SS_DISABLE != 0 {
            stack = get_stack();
            sigaltstack(&stack, ptr::null_mut());
            Handler { data: stack.ss_sp as *mut libc::c_void }
        } else {
            Handler::null()
        }
    }

    pub unsafe fn drop_handler(data: *mut libc::c_void) {
        if !data.is_null() {
            let sigstack_size = sigstack_size();
            let page_size = PAGE_SIZE.load(Ordering::Relaxed);
            let stack = libc::stack_t {
                ss_sp: ptr::null_mut(),
                ss_flags: SS_DISABLE,
                // Workaround for bug in macOS implementation of sigaltstack
                // UNIX2003 which returns ENOMEM when disabling a stack while
                // passing ss_size smaller than MINSIGSTKSZ. According to POSIX
                // both ss_sp and ss_size should be ignored in this case.
                ss_size: sigstack_size,
            };
            sigaltstack(&stack, ptr::null_mut());
            // We know from `get_stackp` that the alternate stack we installed is part of a mapping
            // that started one page earlier, so walk back a page and unmap from there.
            munmap(data.sub(page_size), sigstack_size + page_size);
        }
    }

    /// Modern kernels on modern hardware can have dynamic signal stack sizes.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn sigstack_size() -> usize {
        // FIXME: reuse const from libc when available?
        const AT_MINSIGSTKSZ: crate::ffi::c_ulong = 51;
        let dynamic_sigstksz = unsafe { libc::getauxval(AT_MINSIGSTKSZ) };
        // If getauxval couldn't find the entry, it returns 0,
        // so take the higher of the "constant" and auxval.
        // This transparently supports older kernels which don't provide AT_MINSIGSTKSZ
        libc::SIGSTKSZ.max(dynamic_sigstksz as _)
    }

    /// Not all OS support hardware where this is needed.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn sigstack_size() -> usize {
        libc::SIGSTKSZ
    }

    #[cfg(target_os = "solaris")]
    unsafe fn get_stack_start() -> Option<*mut libc::c_void> {
        let mut current_stack: libc::stack_t = crate::mem::zeroed();
        assert_eq!(libc::stack_getbounds(&mut current_stack), 0);
        Some(current_stack.ss_sp)
    }

    #[cfg(target_os = "macos")]
    unsafe fn get_stack_start() -> Option<*mut libc::c_void> {
        let th = libc::pthread_self();
        let stackptr = libc::pthread_get_stackaddr_np(th);
        Some(stackptr.map_addr(|addr| addr - libc::pthread_get_stacksize_np(th)))
    }

    #[cfg(target_os = "openbsd")]
    unsafe fn get_stack_start() -> Option<*mut libc::c_void> {
        let mut current_stack: libc::stack_t = crate::mem::zeroed();
        assert_eq!(libc::pthread_stackseg_np(libc::pthread_self(), &mut current_stack), 0);

        let stack_ptr = current_stack.ss_sp;
        let stackaddr = if libc::pthread_main_np() == 1 {
            // main thread
            stack_ptr.addr() - current_stack.ss_size + PAGE_SIZE.load(Ordering::Relaxed)
        } else {
            // new thread
            stack_ptr.addr() - current_stack.ss_size
        };
        Some(stack_ptr.with_addr(stackaddr))
    }

    #[cfg(any(
        target_os = "android",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "hurd",
        target_os = "linux",
        target_os = "l4re"
    ))]
    unsafe fn get_stack_start() -> Option<*mut libc::c_void> {
        let mut ret = None;
        let mut attr: libc::pthread_attr_t = crate::mem::zeroed();
        #[cfg(target_os = "freebsd")]
        assert_eq!(libc::pthread_attr_init(&mut attr), 0);
        #[cfg(target_os = "freebsd")]
        let e = libc::pthread_attr_get_np(libc::pthread_self(), &mut attr);
        #[cfg(not(target_os = "freebsd"))]
        let e = libc::pthread_getattr_np(libc::pthread_self(), &mut attr);
        if e == 0 {
            let mut stackaddr = crate::ptr::null_mut();
            let mut stacksize = 0;
            assert_eq!(libc::pthread_attr_getstack(&attr, &mut stackaddr, &mut stacksize), 0);
            ret = Some(stackaddr);
        }
        if e == 0 || cfg!(target_os = "freebsd") {
            assert_eq!(libc::pthread_attr_destroy(&mut attr), 0);
        }
        ret
    }

    unsafe fn get_stack_start_aligned() -> Option<*mut libc::c_void> {
        let page_size = PAGE_SIZE.load(Ordering::Relaxed);
        let stackptr = get_stack_start()?;
        let stackaddr = stackptr.addr();

        // Ensure stackaddr is page aligned! A parent process might
        // have reset RLIMIT_STACK to be non-page aligned. The
        // pthread_attr_getstack() reports the usable stack area
        // stackaddr < stackaddr + stacksize, so if stackaddr is not
        // page-aligned, calculate the fix such that stackaddr <
        // new_page_aligned_stackaddr < stackaddr + stacksize
        let remainder = stackaddr % page_size;
        Some(if remainder == 0 {
            stackptr
        } else {
            stackptr.with_addr(stackaddr + page_size - remainder)
        })
    }

    unsafe fn install_main_guard() -> Option<Range<usize>> {
        let page_size = PAGE_SIZE.load(Ordering::Relaxed);
        if cfg!(all(target_os = "linux", not(target_env = "musl"))) {
            // Linux doesn't allocate the whole stack right away, and
            // the kernel has its own stack-guard mechanism to fault
            // when growing too close to an existing mapping. If we map
            // our own guard, then the kernel starts enforcing a rather
            // large gap above that, rendering much of the possible
            // stack space useless. See #43052.
            //
            // Instead, we'll just note where we expect rlimit to start
            // faulting, so our handler can report "stack overflow", and
            // trust that the kernel's own stack guard will work.
            let stackptr = get_stack_start_aligned()?;
            let stackaddr = stackptr.addr();
            Some(stackaddr - page_size..stackaddr)
        } else if cfg!(all(target_os = "linux", target_env = "musl")) {
            // For the main thread, the musl's pthread_attr_getstack
            // returns the current stack size, rather than maximum size
            // it can eventually grow to. It cannot be used to determine
            // the position of kernel's stack guard.
            None
        } else if cfg!(target_os = "freebsd") {
            // FreeBSD's stack autogrows, and optionally includes a guard page
            // at the bottom. If we try to remap the bottom of the stack
            // ourselves, FreeBSD's guard page moves upwards. So we'll just use
            // the builtin guard page.
            let stackptr = get_stack_start_aligned()?;
            let guardaddr = stackptr.addr();
            // Technically the number of guard pages is tunable and controlled
            // by the security.bsd.stack_guard_page sysctl.
            // By default it is 1, checking once is enough since it is
            // a boot time config value.
            static PAGES: crate::sync::OnceLock<usize> = crate::sync::OnceLock::new();

            let pages = PAGES.get_or_init(|| {
                use crate::sys::weak::dlsym;
                dlsym!(fn sysctlbyname(*const libc::c_char, *mut libc::c_void, *mut libc::size_t, *const libc::c_void, libc::size_t) -> libc::c_int);
                let mut guard: usize = 0;
                let mut size = crate::mem::size_of_val(&guard);
                let oid = crate::ffi::CStr::from_bytes_with_nul(
                    b"security.bsd.stack_guard_page\0",
                )
                .unwrap();
                match sysctlbyname.get() {
                    Some(fcn) => {
                        if fcn(oid.as_ptr(), core::ptr::addr_of_mut!(guard) as *mut _, core::ptr::addr_of_mut!(size) as *mut _, crate::ptr::null_mut(), 0) == 0 {
                            guard
                        } else {
                            1
                        }
                    },
                    _ => 1,
                }
            });
            Some(guardaddr..guardaddr + pages * page_size)
        } else if cfg!(any(target_os = "openbsd", target_os = "netbsd")) {
            // OpenBSD stack already includes a guard page, and stack is
            // immutable.
            // NetBSD stack includes the guard page.
            //
            // We'll just note where we expect rlimit to start
            // faulting, so our handler can report "stack overflow", and
            // trust that the kernel's own stack guard will work.
            let stackptr = get_stack_start_aligned()?;
            let stackaddr = stackptr.addr();
            Some(stackaddr - page_size..stackaddr)
        } else {
            // Reallocate the last page of the stack.
            // This ensures SIGBUS will be raised on
            // stack overflow.
            // Systems which enforce strict PAX MPROTECT do not allow
            // to mprotect() a mapping with less restrictive permissions
            // than the initial mmap() used, so we mmap() here with
            // read/write permissions and only then mprotect() it to
            // no permissions at all. See issue #50313.
            let stackptr = get_stack_start_aligned()?;
            let result = mmap64(
                stackptr,
                page_size,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANON | MAP_FIXED,
                -1,
                0,
            );
            if result != stackptr || result == MAP_FAILED {
                panic!("failed to allocate a guard page: {}", io::Error::last_os_error());
            }

            let result = mprotect(stackptr, page_size, PROT_NONE);
            if result != 0 {
                panic!("failed to protect the guard page: {}", io::Error::last_os_error());
            }

            let guardaddr = stackptr.addr();

            Some(guardaddr..guardaddr + page_size)
        }
    }

    #[cfg(any(target_os = "macos", target_os = "openbsd", target_os = "solaris"))]
    unsafe fn current_guard() -> Option<Range<usize>> {
        let stackptr = get_stack_start()?;
        let stackaddr = stackptr.addr();
        Some(stackaddr - PAGE_SIZE.load(Ordering::Relaxed)..stackaddr)
    }

    #[cfg(any(
        target_os = "android",
        target_os = "freebsd",
        target_os = "hurd",
        target_os = "linux",
        target_os = "netbsd",
        target_os = "l4re"
    ))]
    unsafe fn current_guard() -> Option<Range<usize>> {
        let mut ret = None;
        let mut attr: libc::pthread_attr_t = crate::mem::zeroed();
        #[cfg(target_os = "freebsd")]
        assert_eq!(libc::pthread_attr_init(&mut attr), 0);
        #[cfg(target_os = "freebsd")]
        let e = libc::pthread_attr_get_np(libc::pthread_self(), &mut attr);
        #[cfg(not(target_os = "freebsd"))]
        let e = libc::pthread_getattr_np(libc::pthread_self(), &mut attr);
        if e == 0 {
            let mut guardsize = 0;
            assert_eq!(libc::pthread_attr_getguardsize(&attr, &mut guardsize), 0);
            if guardsize == 0 {
                if cfg!(all(target_os = "linux", target_env = "musl")) {
                    // musl versions before 1.1.19 always reported guard
                    // size obtained from pthread_attr_get_np as zero.
                    // Use page size as a fallback.
                    guardsize = PAGE_SIZE.load(Ordering::Relaxed);
                } else {
                    panic!("there is no guard page");
                }
            }
            let mut stackptr = crate::ptr::null_mut::<libc::c_void>();
            let mut size = 0;
            assert_eq!(libc::pthread_attr_getstack(&attr, &mut stackptr, &mut size), 0);

            let stackaddr = stackptr.addr();
            ret = if cfg!(any(target_os = "freebsd", target_os = "netbsd", target_os = "hurd")) {
                Some(stackaddr - guardsize..stackaddr)
            } else if cfg!(all(target_os = "linux", target_env = "musl")) {
                Some(stackaddr - guardsize..stackaddr)
            } else if cfg!(all(target_os = "linux", any(target_env = "gnu", target_env = "uclibc")))
            {
                // glibc used to include the guard area within the stack, as noted in the BUGS
                // section of `man pthread_attr_getguardsize`. This has been corrected starting
                // with glibc 2.27, and in some distro backports, so the guard is now placed at the
                // end (below) the stack. There's no easy way for us to know which we have at
                // runtime, so we'll just match any fault in the range right above or below the
                // stack base to call that fault a stack overflow.
                Some(stackaddr - guardsize..stackaddr + guardsize)
            } else {
                Some(stackaddr..stackaddr + guardsize)
            };
        }
        if e == 0 || cfg!(target_os = "freebsd") {
            assert_eq!(libc::pthread_attr_destroy(&mut attr), 0);
        }
        ret
    }
}

// This is intentionally not enabled on iOS/tvOS/watchOS/visionOS, as it uses
// several symbols that might lead to rejections from the App Store, namely
// `sigaction`, `sigaltstack`, `sysctlbyname`, `mmap`, `munmap` and `mprotect`.
//
// This might be overly cautious, though it is also what Swift does (and they
// usually have fewer qualms about forwards compatibility, since the runtime
// is shipped with the OS):
// <https://github.com/apple/swift/blob/swift-5.10-RELEASE/stdlib/public/runtime/CrashHandlerMacOS.cpp>
#[cfg(not(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "hurd",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "solaris"
)))]
mod imp {
    pub unsafe fn init() {}

    pub unsafe fn cleanup() {}

    pub unsafe fn make_handler(_main_thread: bool) -> super::Handler {
        super::Handler::null()
    }

    pub unsafe fn drop_handler(_data: *mut libc::c_void) {}
}
