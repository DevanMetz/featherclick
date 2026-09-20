//! Embeds the icon and version metadata into the Windows executable.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/icon.ico");
        resource.set("ProductName", "FeatherClick");
        resource.set(
            "FileDescription",
            "FeatherClick - cross-platform autoclicker",
        );
        resource.set("LegalCopyright", "MIT licensed");
        // A missing resource compiler should not fail the build; the binary
        // simply keeps the default icon.
        if let Err(err) = resource.compile() {
            println!("cargo:warning=skipping Windows resources: {err}");
        }
    }
}
