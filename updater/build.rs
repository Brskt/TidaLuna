fn main() {
    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("../tidaluna.ico");
        res.compile().expect("winres compile");
    }
}
