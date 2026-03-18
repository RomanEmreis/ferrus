fn main() {
    unsafe {
        let ret = libc::kill(u32::MAX as i32, 0);
        let errno = *libc::__errno_location();
        println!("ret={}, errno={}, ESRCH={}, EPERM={}", ret, errno, libc::ESRCH, libc::EPERM);
    }
}
