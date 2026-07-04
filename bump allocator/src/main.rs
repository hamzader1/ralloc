fn main() {
    unsafe {
        let a: *mut u8 = std::ptr::null_mut();
        a.write(1);
        // *a = 1;
        // let sneaky = (a as usize) as *mut u8;
        // *sneaky = 99; // same address as a, different provenance
        // let x = *a; // compiler cached 1 in a register — wrong answer now
        // dbg!(x);
    }
}
