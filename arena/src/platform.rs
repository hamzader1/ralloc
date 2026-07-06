use core::ptr::null_mut;
use libc::_SC_PAGE_SIZE;
use libc::MAP_ANONYMOUS;
use libc::MAP_FAILED;
use libc::MAP_PRIVATE;
use libc::PROT_READ;
use libc::PROT_WRITE;
use libc::c_int;
use libc::c_void;
use libc::munmap;
use libc::off_t;
use libc::sysconf;

const FLAG: c_int = MAP_PRIVATE | MAP_ANONYMOUS;
const PROT: c_int = PROT_READ | PROT_WRITE;
const FD: c_int = -1;
const OFFSET: off_t = 0;

pub struct Platform;
impl Platform {
    pub fn get_page_size() -> usize {
        unsafe { sysconf(_SC_PAGE_SIZE) as usize }
    }

    pub fn mmap(size: usize) -> *mut u8 {
        unsafe {
            let ptr = libc::mmap(null_mut(), size, PROT, FLAG, FD, OFFSET);
            if ptr == MAP_FAILED {
                eprintln!("mmap failed: {}", std::io::Error::last_os_error());
                null_mut()
            } else {
                ptr as *mut u8
            }
        }
    }
    pub fn munmap<T>(addr: *mut T, size: usize) {
        unsafe {
            munmap(addr as *mut c_void, size);
        }
    }
}
