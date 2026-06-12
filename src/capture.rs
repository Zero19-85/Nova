use windows::core::{Result, Interface};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Foundation::HMODULE;

pub struct DesktopCapturer {
    pub dupl: IDXGIOutputDuplication,
    pub device: ID3D11Device,
    /// Native resolution of the duplicated output (DXGI_OUTPUT_DESC.DesktopCoordinates).
    /// The encoder/color-conversion pipeline must be initialized with these
    /// dimensions — anything else leaves the video processor's input size
    /// mismatched with the captured texture, which blits only the overlapping
    /// region and leaves the rest of the NV12 surface (typically the bottom)
    /// black/uninitialized.
    pub width: u32,
    pub height: u32,
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

            let desc = output.GetDesc()?;
            let rect = desc.DesktopCoordinates;
            let width  = (rect.right - rect.left) as u32;
            let height = (rect.bottom - rect.top) as u32;

            let output1: IDXGIOutput1 = output.cast()?;
            let dupl = output1.DuplicateOutput(&dxgi_device)?;

            println!("✅ DXGI Desktop Duplication READY! ({}x{})", width, height);

            Ok(Self { dupl, device, width, height })
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

    /// Fetches the new cursor shape after `acquire_frame()` reports
    /// `frame_info.PointerShapeBufferSize > 0`. Returns the raw shape bytes
    /// (MONOCHROME AND/XOR masks, or a BGRA bitmap for COLOR/MASKED_COLOR)
    /// plus the accompanying `DXGI_OUTDUPL_POINTER_SHAPE_INFO`.
    pub fn get_pointer_shape(&self, buffer_size: u32) -> Result<(Vec<u8>, DXGI_OUTDUPL_POINTER_SHAPE_INFO)> {
        unsafe {
            let mut buffer = vec![0u8; buffer_size as usize];
            let mut required_size = 0u32;
            let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            self.dupl.GetFramePointerShape(
                buffer_size,
                buffer.as_mut_ptr() as *mut _,
                &mut required_size,
                &mut shape_info,
            )?;
            Ok((buffer, shape_info))
        }
    }
}