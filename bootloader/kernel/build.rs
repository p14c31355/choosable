fn main() {
    // Stage2 loads the kernel flat binary at physical address 0x9E00
    // and far-jumps into it.  The kernel must be linked at that address.
    println!("cargo:rustc-link-arg=-Ttext=0x9E00");
}