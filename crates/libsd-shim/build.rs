fn main() {
    // Programs link against libsystemd.so.0.
    println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libsystemd.so.0");
}
