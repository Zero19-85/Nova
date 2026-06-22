fn main() {
    cc::Build::new()
        .cpp(true)
        .file("shim/shim.cpp")
        .file("shim/NvEncoder/NvEncoder.cpp")
        .file("shim/NvEncoder/NvEncoderD3D11.cpp")
        .file("shim/audio_shim.cpp")
        .include("shim")
        .include("shim/NvEncoder")
        .include("shim/include")
        .flag("/std:c++17")
        .define("WIN32", None)
        .define("_WIN32", None)
        .define("WIN32_LEAN_AND_MEAN", None)
        .compile("nova_shim");

    // 1. Link our compiled C++ shim
    println!("cargo:rustc-link-lib=static=nova_shim");
    
    // 2. THE FIX: Tell the linker where the NVIDIA SDK library folder is
    println!("cargo:rustc-link-search=native=C:/NVSDK/Lib/win/x64");
    
    // 3. THE FIX: Tell the linker to glue nvencodeapi.lib into our executable
    println!("cargo:rustc-link-lib=nvencodeapi");
    
    // ole32: CoCreateInstance, CoInitializeEx, CoTaskMemFree (WASAPI shim)
    println!("cargo:rustc-link-lib=ole32");

    // d3dcompiler: D3DCompile, used at runtime to build the cursor-overlay
    // shaders. d3dcompiler_47.dll ships with Windows itself — no redist needed.
    println!("cargo:rustc-link-lib=d3dcompiler");

    // Tell Cargo to re-compile automatically if any of these files change
    println!("cargo:rerun-if-changed=shim/audio_shim.cpp");
    println!("cargo:rerun-if-changed=shim/shim.cpp");
    println!("cargo:rerun-if-changed=shim/NvEncoder/NvEncoder.cpp");
    println!("cargo:rerun-if-changed=shim/NvEncoder/NvEncoderD3D11.cpp");
    println!("cargo:rerun-if-changed=shim/NvEncoder/NvEncoder.h");
    println!("cargo:rerun-if-changed=shim/NvEncoder/NvEncoderD3D11.h");
    println!("cargo:rerun-if-changed=shim/include/nvEncodeAPI.h");

    // Require administrator privileges from process launch (embedded UAC
    // manifest, compiled to a .res via rc.exe and linked as a resource).
    // virtual_display.rs's set_enabled() needs DIF_PROPERTYCHANGE/
    // DICS_ENABLE/DISABLE rights to toggle the Root\MttVDD devnode; without
    // this, that call fails with ERROR_ACCESS_DENIED and used to fall back
    // to an interactive `devcon`+UAC prompt mid-session (stalling
    // activate_for_stream if the prompt isn't answered immediately). With
    // the token elevated from boot, the native SetupDi* path always
    // succeeds and that fallback is gone.
    //
    // A plain `/MANIFESTUAC:...` linker flag was tried first, but the
    // spaced `level='...' uiAccess='...'` value gets mangled somewhere in
    // cargo -> rustc -> link.exe argument passing and produces a binary
    // with no resource section at all. Compiling the manifest into a .res
    // ourselves sidesteps that entirely.
    //
    // Scoped to the nova-server binary only (rustc-link-arg-bin) so the
    // `cargo test` harness binary — a different link target — stays
    // unelevated and `cargo test` doesn't prompt for UAC.
    if std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "msvc" {
        let out_dir = std::env::var("OUT_DIR").unwrap();
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let manifest_path = format!("{manifest_dir}/nova-server.manifest").replace('\\', "/");
        let rc_path = std::path::Path::new(&out_dir).join("nova-server.rc");
        let res_path = std::path::Path::new(&out_dir).join("nova-server.res");

        // Resource ID 1, type 24 (RT_MANIFEST) — the well-known
        // CREATEPROCESS_MANIFEST_RESOURCE_ID the OS loader looks for.
        // Resource ID 1, type ICON (RT_GROUP_ICON=14) — Windows Explorer
        // picks the first ICON group resource as the file's visible icon.
        // Both can share resource ID 1 because they have different types.
        let ico_path = format!("{manifest_dir}/assets/Nova.ico").replace('\\', "/");
        let icon_line = if std::path::Path::new(&format!("{manifest_dir}/assets/Nova.ico")).exists() {
            format!("1 ICON \"{ico_path}\"\n")
        } else {
            String::new()
        };
        std::fs::write(&rc_path, format!("1 24 \"{manifest_path}\"\n{icon_line}"))
            .expect("failed to write nova-server.rc");
        println!("cargo:rerun-if-changed=assets/Nova.ico");

        let target = std::env::var("TARGET").unwrap();
        let rc_exe = cc::windows_registry::find_tool(&target, "rc.exe")
            .map(|t| t.path().to_path_buf())
            .unwrap_or_else(|| find_rc_exe_fallback());

        let status = std::process::Command::new(&rc_exe)
            .arg("/nologo")
            .arg("/fo")
            .arg(&res_path)
            .arg(&rc_path)
            .status()
            .expect("failed to run rc.exe");
        assert!(status.success(), "rc.exe failed to compile UAC manifest resource");

        println!("cargo:rustc-link-arg-bin=nova-server={}", res_path.display());
        println!("cargo:rerun-if-changed=nova-server.manifest");
    }
}

/// Locates rc.exe directly under the Windows Kits install directory when
/// `cc::windows_registry::find_tool` doesn't turn it up.
fn find_rc_exe_fallback() -> std::path::PathBuf {
    let base = std::path::Path::new("C:/Program Files (x86)/Windows Kits/10/bin");
    let mut versions: Vec<_> = std::fs::read_dir(base)
        .expect("Windows Kits 10 bin directory not found — is the Windows SDK installed?")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    versions.sort();
    let latest = versions.last().expect("no Windows Kits versions found");
    latest.join("x64").join("rc.exe")
}