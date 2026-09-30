// Linux: ffmpeg-sys-next links the static FFmpeg libraries from FFMPEG_DIR but not what they
// depend on; that list comes from FFmpeg's own pkg-config files (glslang and libstdc++ for
// scale_vulkan, libva/libdrm dynamic from the system). As link arguments, which go last: a
// `rustc-link-lib` of this crate would come before FFmpeg's static libraries that need them.
fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    let dir = std::env::var("FFMPEG_DIR").expect("FFMPEG_DIR: run scripts/deps/ffmpeg.sh first");
    let out = std::process::Command::new("pkg-config")
        .args(["--static", "--libs", "libavfilter", "libavcodec", "libavutil"])
        .env("PKG_CONFIG_LIBDIR", format!("{dir}/lib/pkgconfig:{dir}/share/pkgconfig"))
        .output()
        .expect("pkg-config");
    assert!(out.status.success(), "pkg-config: {}", String::from_utf8_lossy(&out.stderr));
    for flag in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        if let Some(path) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        } else if (flag.starts_with("-l") || flag == "-pthread")
            && !["-lavfilter", "-lavcodec", "-lavutil"].contains(&flag)
        {
            println!("cargo:rustc-link-arg={flag}");
        }
    }
}
