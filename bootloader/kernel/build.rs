fn main() {
    // Stage2 loads the kernel flat binary at physical address 0x9E00,
    // copies it to 0x100000 (1 MiB), and far-jumps into it.
    // The kernel MUST be linked at its final destination (0x100000),
    // not its intermediate loading address.
    // The bootloader's build.rs also sets RUSTFLAGS=-Ttext=0x100000
    // when cross-compiling; this build.rs ensures the same address
    // is used when building the kernel standalone.
    println!("cargo:rustc-link-arg=-Ttext=0x100000");
}
