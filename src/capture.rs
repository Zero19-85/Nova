use windows::core::{Result, Interface};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Foundation::HMODULE;

pub struct DesktopCapturer {
    pub dupl: IDXGIOutputDuplication,
    pub device: ID3D11Device,
}

impl DesktopCapturer {
    pub fn new() -> Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;

            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;

            let device = device.expect("Failed to create D3D11 device");

            let dxgi_device: IDXGIDevice = device.cast()?;
            let adapter = dxgi_device.GetAdapter()?;
            let output = adapter.EnumOutputs(0)?;
            let output1: IDXGIOutput1 = output.cast()?;
            let dupl = output1.DuplicateOutput(&dxgi_device)?;

            println!("✅ DXGI Desktop Duplication READY!");

            Ok(Self { dupl, device })
        }
    }

    pub fn acquire_frame(&self) -> Result<(IDXGIResource, DXGI_OUTDUPL_FRAME_INFO)> {
        unsafe {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            self.dupl.AcquireNextFrame(1000, &mut frame_info, &mut resource)?;
            Ok((resource.expect("No resource"), frame_info))
        }
    }

    pub fn release_frame(&self) -> Result<()> {
        unsafe { self.dupl.ReleaseFrame()?; }
        Ok(())
    }

    pub fn get_texture(&self, resource: &IDXGIResource) -> Result<ID3D11Texture2D> {
        resource.cast()
    }
}