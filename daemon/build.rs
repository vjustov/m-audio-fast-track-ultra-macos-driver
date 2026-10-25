fn main() {
    println!("cargo:rerun-if-changed=src/xpc_shim.c");
    println!("cargo:rerun-if-changed=src/power_shim.c");
    println!("cargo:rerun-if-changed=../shared/ftu_shm.h");
    cc::Build::new().file("src/xpc_shim.c").file("src/power_shim.c").flag("-fblocks").warnings(true).compile("shims");
    println!("cargo:rustc-link-lib=framework=IOKit");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
}
