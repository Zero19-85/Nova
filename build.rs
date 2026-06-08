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

    // Tell Cargo to re-compile automatically if any of these files change
    println!("cargo:rerun-if-changed=shim/audio_shim.cpp");
    println!("cargo:rerun-if-changed=shim/shim.cpp");
    println!("cargo:rerun-if-changed=shim/NvEncoder/NvEncoder.cpp");
    println!("cargo:rerun-if-changed=shim/NvEncoder/NvEncoderD3D11.cpp");
    println!("cargo:rerun-if-changed=shim/NvEncoder/NvEncoder.h");
    println!("cargo:rerun-if-changed=shim/NvEncoder/NvEncoderD3D11.h");
    println!("cargo:rerun-if-changed=shim/include/nvEncodeAPI.h");
}