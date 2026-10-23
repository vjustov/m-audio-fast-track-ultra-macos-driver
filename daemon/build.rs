fn main() {
    println!("cargo:rerun-if-changed=src/xpc_shim.c");
    println!("cargo:rerun-if-changed=../shared/ftu_shm.h");
    cc::Build::new().file("src/xpc_shim.c").flag("-fblocks").warnings(true).compile("xpc_shim");
}
