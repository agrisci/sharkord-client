// ffmpeg-sys-next links the static FFmpeg libraries from FFMPEG_DIR but not what they depend on;
// that list comes from FFmpeg's own pkg-config files.
// Linux: glslang and libstdc++ for scale_vulkan, libva/libdrm dynamic from the system. As link
// arguments, which go last: a `rustc-link-lib` of this crate would come before FFmpeg's static
// libraries that need them.
// Windows (MSVC): the system libraries (D3D11, DXGI, COM, ...) and libvpl, as link libraries (the
// MSVC linker resolves in any order). The .pc files carry MSYS2 paths: the prefix is given here.
fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "linux" && os != "windows" {
        return;
    }
    let dir = std::env::var("FFMPEG_DIR").expect("FFMPEG_DIR: run scripts/deps/ffmpeg.sh (Linux) or ffmpeg-windows.sh first");
    let windows = os == "windows";
    let dir = if windows { std::fs::canonicalize(&dir).map(|p| p.display().to_string().trim_start_matches(r"\\?\").to_owned()).unwrap_or(dir) } else { dir };
    let sep = if windows { ';' } else { ':' };
    let mut cmd = std::process::Command::new("pkg-config");
    cmd.args(["--static", "--libs", "libavfilter", "libavcodec", "libavutil"])
        .env("PKG_CONFIG_LIBDIR", format!("{dir}/lib/pkgconfig{sep}{dir}/share/pkgconfig"));
    if windows {
        cmd.arg(format!("--define-variable=prefix={dir}"));
    }
    let out = cmd.output().expect("pkg-config");
    assert!(out.status.success(), "pkg-config: {}", String::from_utf8_lossy(&out.stderr));
    for flag in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        if flag.to_ascii_lowercase().starts_with("-libpath:") {
            continue;   // an MSYS2 path; the prefix's lib/ is added below
        } else if let Some(path) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        } else if ["-lavfilter", "-lavcodec", "-lavutil"].contains(&flag) {
            continue;
        } else if windows {
            // gfxcapture's C++ runtime: MSVC links its own (the static CRT), there is no stdc++.lib
            if flag == "-lstdc++" {
                continue;
            }
            if let Some(lib) = flag.strip_prefix("-l").or_else(|| flag.strip_suffix(".lib")) {
                println!("cargo:rustc-link-lib={lib}");
            }
        } else if flag.starts_with("-l") || flag == "-pthread" {
            println!("cargo:rustc-link-arg={flag}");
        }
    }
    if windows {
        println!("cargo:rustc-link-search=native={dir}/lib");
    }
}
