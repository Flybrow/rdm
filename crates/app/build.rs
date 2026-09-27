fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/rdm.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/rdm.ico").set("ProductName", "RDM").set("FileDescription", "Rust Download Manager");
        res.compile().expect("embed Windows resources");
    }
}
