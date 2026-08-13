use std::path::{Path, PathBuf};

fn main() {
    let out_dir      = std::env::var("OUT_DIR").unwrap();
    let out_path     = PathBuf::from(&out_dir);
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let target       = std::env::var("TARGET").unwrap();

    // OUT_DIR = target/{profile}/build/{pkg}-{hash}/out
    // Walk three levels up to reach target/{profile}/
    let profile_dir = out_path
        .parent().unwrap()   // out  → {pkg}-{hash}
        .parent().unwrap()   // {pkg}-{hash} → build
        .parent().unwrap();  // build → {profile}  (target/release or target/debug)

    let dll_dest = profile_dir.join("nova_shim.dll");

    // ── Diagnostic: visible in `cargo build` output ───────────────────────────
    println!("cargo:warning=DLL Path: {:?}", dll_dest);

    // ── Build nova_shim.dll from the C++ shim ─────────────────────────────────
    // Compiles shim sources with cl.exe, links with /DLL via link.exe, then
    // copies nova_shim.dll to target/{profile}/.  The import lib (nova_shim.lib)
    // stays in OUT_DIR so the Rust linker can find it via the search directive
    // below.
    build_shim_dll(&target, &manifest_dir, &out_path, &dll_dest);

    // ── Rust linker directives ────────────────────────────────────────────────
    // Link against the import library produced above (resolves the extern "C"
    // symbols in encoder.rs / audio_shim.cpp at link time; the DLL is loaded at
    // runtime from the exe directory).
    println!("cargo:rustc-link-search=native={}", out_dir);
    println!("cargo:rustc-link-lib=dylib=nova_shim");

    // ole32: CoCreateInstance / CoInitializeEx used by the `windows` crate for
    // WASAPI and WinRT on the Rust side (separate from the DLL's own ole32 use).
    println!("cargo:rustc-link-lib=ole32");

    // ── Rerun triggers ────────────────────────────────────────────────────────
    println!("cargo:rerun-if-changed=shim/shim.cpp");
    println!("cargo:rerun-if-changed=shim/audio_shim.cpp");
    println!("cargo:rerun-if-changed=shim/NvEncoder/NvEncoder.cpp");
    println!("cargo:rerun-if-changed=shim/NvEncoder/NvEncoderD3D11.cpp");
    println!("cargo:rerun-if-changed=shim/NvEncoder/NvEncoder.h");
    println!("cargo:rerun-if-changed=shim/NvEncoder/NvEncoderD3D11.h");
    println!("cargo:rerun-if-changed=shim/include/nvEncodeAPI.h");

    // ── UAC manifest + app icon (nova-server binary only) ────────────────────
    if std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "msvc" {
        let manifest_path = format!("{manifest_dir}/nova-server.manifest").replace('\\', "/");
        let rc_path  = out_path.join("nova-server.rc");
        let res_path = out_path.join("nova-server.res");

        let ico_path  = format!("{manifest_dir}/assets/Nova.ico").replace('\\', "/");
        let icon_line = if Path::new(&format!("{manifest_dir}/assets/Nova.ico")).exists() {
            format!("1 ICON \"{ico_path}\"\n")
        } else {
            String::new()
        };
        std::fs::write(&rc_path, format!("1 24 \"{manifest_path}\"\n{icon_line}"))
            .expect("failed to write nova-server.rc");
        println!("cargo:rerun-if-changed=assets/Nova.ico");

        let rc_exe = cc::windows_registry::find_tool(&target, "rc.exe")
            .map(|t| t.path().to_path_buf())
            .unwrap_or_else(|| find_rc_exe_fallback());

        let status = std::process::Command::new(&rc_exe)
            .arg("/nologo")
            .arg("/fo").arg(&res_path)
            .arg(&rc_path)
            .status()
            .expect("failed to run rc.exe");
        assert!(status.success(), "rc.exe failed to compile UAC manifest resource");

        println!("cargo:rustc-link-arg-bin=nova-server={}", res_path.display());
        println!("cargo:rerun-if-changed=nova-server.manifest");
    }
}

// ── C++ shim DLL build ────────────────────────────────────────────────────────

fn build_shim_dll(target: &str, manifest_dir: &str, out_dir: &Path, dll_dest: &Path) {
    let cl = cc::windows_registry::find_tool(target, "cl.exe")
        .expect("cl.exe not found — install Visual Studio C++ Build Tools (v143 or later)");
    let link = cc::windows_registry::find_tool(target, "link.exe")
        .expect("link.exe not found — install Visual Studio C++ Build Tools");

    let srcs = [
        "shim/shim.cpp",
        "shim/NvEncoder/NvEncoder.cpp",
        "shim/NvEncoder/NvEncoderD3D11.cpp",
        "shim/audio_shim.cpp",
    ];
    let includes = [
        "shim",
        "shim/NvEncoder",
        "shim/include",
    ];

    // ── Step 1: compile each .cpp → .obj ─────────────────────────────────────
    let mut objs: Vec<PathBuf> = Vec::new();
    for src in &srcs {
        let stem = Path::new(src)
            .file_stem().unwrap()
            .to_str().unwrap();
        let obj = out_dir.join(format!("{stem}.obj"));

        let mut cmd = cl.to_command();
        cmd.arg("/nologo")
           .arg("/c")          // compile only, no link
           .arg("/std:c++17")
           .arg("/EHsc")       // C++ exception handling
           .arg("/O2")         // optimise
           .arg("/MD")         // link DLL-CRT (compatible with Rust's MSVCRT)
           .arg("/DWIN32")
           .arg("/D_WIN32")
           .arg("/DWIN32_LEAN_AND_MEAN");

        for inc in &includes {
            cmd.arg(format!("/I{manifest_dir}/{inc}"));
        }
        cmd.arg(format!("/Fo{}", obj.display()))
           .arg(format!("{manifest_dir}/{src}"));

        println!("cargo:warning=Compiling {src}");
        let status = cmd.status()
            .unwrap_or_else(|e| panic!("Failed to spawn cl.exe for {src}: {e}"));
        assert!(status.success(), "cl.exe failed on {src}");

        objs.push(obj);
    }

    // ── Step 2: link .objs → nova_shim.dll  +  nova_shim.lib (import lib) ───
    let dll_in_out = out_dir.join("nova_shim.dll");
    let implib     = out_dir.join("nova_shim.lib");

    let mut cmd = link.to_command();
    cmd.arg("/nologo")
       .arg("/DLL")
       .arg(format!("/OUT:{}", dll_in_out.display()))
       .arg(format!("/IMPLIB:{}", implib.display()))
       // Windows SDK + MSVC CRT libs come from the env set by find_tool.
       // NVENC SDK: import lib for nvencodeapi.dll (ships with NVIDIA driver).
       .arg("/LIBPATH:C:/NVSDK/Lib/win/x64")
       .arg("nvencodeapi.lib")
       // D3D / DXGI / compiler
       .arg("d3d11.lib")
       .arg("dxgi.lib")
       .arg("d3dcompiler.lib")
       // COM
       .arg("ole32.lib");

    for obj in &objs {
        cmd.arg(obj);
    }

    println!("cargo:warning=Linking nova_shim.dll");
    let status = cmd.status()
        .expect("Failed to spawn link.exe");
    assert!(status.success(), "link.exe failed to produce nova_shim.dll");

    // ── Step 3: copy DLL to target/{profile}/ ────────────────────────────────
    if let Some(parent) = dll_dest.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::copy(&dll_in_out, dll_dest)
        .unwrap_or_else(|e| panic!("Failed to copy nova_shim.dll → {dll_dest:?}: {e}"));

    println!("cargo:warning=nova_shim.dll deployed to {:?}", dll_dest);
}

// ── Fallback: locate rc.exe under the Windows Kits install ───────────────────
fn find_rc_exe_fallback() -> PathBuf {
    let base = Path::new("C:/Program Files (x86)/Windows Kits/10/bin");
    let mut versions: Vec<_> = std::fs::read_dir(base)
        .expect("Windows Kits 10/bin not found — is the Windows SDK installed?")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    versions.sort();
    versions
        .last()
        .expect("No Windows Kits 10 versions found")
        .join("x64/rc.exe")
}
