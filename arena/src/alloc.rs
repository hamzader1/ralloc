#[derive(Debug, Clone, Copy, PartialEq, Eq)]

pub enum AllocatorError {
    OutOfMemory,
    ZeroSizedType,
    AllocationFailed,
    Overflow,
}
impl AllocatorError {
    pub fn panic(&self) -> ! {
        panic!("{}", self.to_string())
    }
}

impl std::fmt::Display for AllocatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfMemory => {
                write!(f, "not enough memory available for allocation")
            }
            Self::ZeroSizedType => {
                write!(f, "cannot allocate a zero-sized type")
            }
            Self::AllocationFailed => {
                write!(f, "memory allocation failed")
            }
            Self::Overflow => {
                write!(f, "arithmetic overflow while calculating allocation size")
            }
        }
    }
}

impl std::error::Error for AllocatorError {}
