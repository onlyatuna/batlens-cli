#[cfg(windows)]
fn main() {
    let mut res = winres::WindowsResource::new();
    res.set(
        "FileDescription",
        "Lightweight native Windows laptop battery monitor",
    );
    res.set("ProductName", "batlens");
    res.set("OriginalFilename", "batlens.exe");
    res.set(
        "LegalCopyright",
        "Copyright (c) 2026 Evan Chen <evanchen940628@gmail.com>. Licensed under MIT.",
    );
    if let Err(e) = res.compile() {
        eprintln!("cargo:warning=Failed to compile Windows resources: {e}");
    }
}

#[cfg(not(windows))]
fn main() {}
