fn main() {
    // The icon lives outside this package; cargo's whole-package default never
    // covered it. This is the only external input: the manifest below is inline.
    println!("cargo:rerun-if-changed=../tidaluna.ico");

    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("../tidaluna.ico");
        // asInvoker: without a manifest, Windows installer-detection elevates any "updater"-named exe (spawn fails 740).
        res.set_manifest(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#,
        );
        res.compile().expect("winres compile");
    }
}
