use core::ptr::null_mut;
use libc::_SC_PAGE_SIZE;
use libc::MAP_ANONYMOUS;
use libc::MAP_FAILED;
use libc::MAP_PRIVATE;
use libc::PROT_READ;
use libc::PROT_WRITE;
use libc::c_int;
use libc::off_t;
use libc::sysconf;

const FLAG: c_int = MAP_PRIVATE | MAP_ANONYMOUS;
const PROT: c_int = PROT_READ | PROT_WRITE;
const FD: c_int = -1;
const OFFSET: off_t = 0;

pub struct Platform;
