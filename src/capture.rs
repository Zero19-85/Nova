use windows::core::{Result, Interface};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Foundation::{E_FAIL, HMODULE};
use std::time::Duration;

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
    /// Copy of the last successfully captured frame, kept on `device` so it
    /// can be re-submitted to the encoder when `AcquireNextFrame` reports
    /// `DXGI_ERROR_WAIT_TIMEOUT` (desktop unchanged). A fully idle/static
    /// desktop — common right after switching to a freshly-activated
    /// virtual display, where nothing has painted yet — can otherwise never
    /// produce a single duplication frame, leaving the stream black
    /// forever. Cleared on `rebind` (resolution/device may have changed).
    last_frame: Option<ID3D11Texture2D>,
}

impl DesktopCapturer {
    pub fn new() -> Result<Self> {
        Self::for_output(None)
    }

    /// Binds to the output whose GDI device name (`DXGI_OUTPUT_DESC::DeviceName`,
    /// e.g. `\\.\DISPLAY28`) matches `gdi_device_name`. When `gdi_device_name`
    /// is `None`, or no output matches, falls back to the first output of the
    /// first adapter — today's default behavior, used at startup before any
    /// virtual-display activation has happened.
    pub fn for_output(gdi_device_name: Option<&str>) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let (adapter, output) = Self::find_output(&factory, gdi_device_name)?;
            let adapter: IDXGIAdapter = adapter.cast()?;

            let mut device = None;
            let mut context = None;

            D3D11CreateDevice(
                Some(&adapter),
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;

            let device = device.expect("Failed to create D3D11 device");

            let desc = output.GetDesc()?;
            let rect = desc.DesktopCoordinates;
            let width  = (rect.right - rect.left) as u32;
            let height = (rect.bottom - rect.top) as u32;

            let dxgi_device: IDXGIDevice = device.cast()?;
            let output1: IDXGIOutput1 = output.cast()?;
            let dupl = output1.DuplicateOutput(&dxgi_device)?;

            println!("✅ DXGI Desktop Duplication READY! ({}x{})", width, height);

            Ok(Self { dupl, device, width, height, last_frame: None })
        }
    }

    /// Re-duplicates the output matching `gdi_device_name` (or the default
    /// output, for `None`), replacing `dupl`/`device`/`width`/`height` in
    /// place. Used both to follow a display-topology change (the new primary
    /// becomes the capture target) and to recover from
    /// `DXGI_ERROR_ACCESS_LOST`, which any `SetDisplayConfig`-class change can
    /// trigger on an existing duplication interface even if the bound output
    /// itself didn't move.
    ///
    /// `DuplicateOutput` (and a cross-adapter `D3D11CreateDevice`) can fail
    /// for a frame or two right after a topology change settles, so this
    /// retries briefly before giving up.
    ///
    /// Returns `Ok(true)` if the caller must recreate its `Encoder`: either
    /// the resolution changed, or the new output lives on a different
    /// adapter and `device` was recreated (the old `Encoder`'s device pointer
    /// would otherwise dangle).
    pub fn rebind(&mut self, gdi_device_name: Option<&str>) -> Result<bool> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let (adapter, output) = Self::find_output(&factory, gdi_device_name)?;

            let desc = output.GetDesc()?;
            let rect = desc.DesktopCoordinates;
            let new_width  = (rect.right - rect.left) as u32;
            let new_height = (rect.bottom - rect.top) as u32;

            let current_dxgi_device: IDXGIDevice = self.device.cast()?;
            let current_adapter: IDXGIAdapter1 = current_dxgi_device.GetAdapter()?.cast()?;
            let same_adapter = {
                let a = current_adapter.GetDesc1()?.AdapterLuid;
                let b = adapter.GetDesc1()?.AdapterLuid;
                a.LowPart == b.LowPart && a.HighPart == b.HighPart
            };

            let mut last_err = None;
            for _ in 0..20 {
                let attempt: Result<(IDXGIOutputDuplication, ID3D11Device)> = if same_adapter {
                    let output1: IDXGIOutput1 = output.cast()?;
                    output1.DuplicateOutput(&current_dxgi_device).map(|d| (d, self.device.clone()))
                } else {
                    let adapter_base: IDXGIAdapter = adapter.cast()?;
                    let mut device = None;
                    let mut context = None;
                    D3D11CreateDevice(
                        Some(&adapter_base),
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                        Some(&[D3D_FEATURE_LEVEL_11_1]),
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        Some(&mut context),
                    ).and_then(|()| {
                        let device = device.expect("Failed to create D3D11 device");
                        let dxgi_device: IDXGIDevice = device.cast()?;
                        let output1: IDXGIOutput1 = output.cast()?;
                        output1.DuplicateOutput(&dxgi_device).map(|d| (d, device))
                    })
                };

                match attempt {
                    Ok((dupl, device)) => {
                        let resized = new_width != self.width || new_height != self.height || !same_adapter;
                        self.dupl   = dupl;
                        self.device = device;
                        self.width  = new_width;
                        self.height = new_height;
                        self.last_frame = None;
                        println!("✅ DXGI Desktop Duplication re-bound ({}x{})", new_width, new_height);
                        return Ok(resized);
                    }
                    Err(e) => {
                        last_err = Some(e);
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
            Err(last_err.unwrap())
        }
    }

    /// Walks every output of every adapter looking for one whose
    /// `DXGI_OUTPUT_DESC::DeviceName` matches `gdi_device_name`. Always also
    /// records the first output it sees as a fallback, returned when
    /// `gdi_device_name` is `None` or doesn't match anything (e.g. the
    /// virtual display hasn't appeared in DXGI's enumeration yet).
    fn find_output(factory: &IDXGIFactory1, gdi_device_name: Option<&str>) -> Result<(IDXGIAdapter1, IDXGIOutput)> {
        let mut fallback: Option<(IDXGIAdapter1, IDXGIOutput)> = None;

        let mut i = 0;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
            let mut j = 0;
            while let Ok(output) = unsafe { adapter.EnumOutputs(j) } {
                let desc = unsafe { output.GetDesc()? };
                let name = String::from_utf16_lossy(&desc.DeviceName);
                let name = name.trim_end_matches('\0');

                if let Some(target) = gdi_device_name {
                    if name == target {
                        return Ok((adapter, output));
                    }
                }
                if fallback.is_none() {
                    fallback = Some((adapter.clone(), output.clone()));
                }
                j += 1;
            }
            i += 1;
        }

        match fallback {
            Some(fb) => {
                if let Some(target) = gdi_device_name {
                    println!("⚠️  No DXGI output matches GDI device {target} — falling back to the first available output");
                }
                Ok(fb)
            }
            None => Err(E_FAIL.into()),
        }
    }

    /// `timeout_ms` should be roughly the target frame interval (e.g. ~16ms
    /// at 60fps), not a generous "wait forever" value. A virtual display
    /// (IDD) with nothing painting to it can sit in WAIT_TIMEOUT forever —
    /// at a 1000ms timeout that caps the whole capture loop (and therefore
    /// the duplicate-frame replay in lib.rs) to ~1fps, far below what the
    /// client negotiated, which Moonlight reports as a poor network
    /// connection and disconnects over.
    pub fn acquire_frame(&self, timeout_ms: u32) -> Result<(IDXGIResource, DXGI_OUTDUPL_FRAME_INFO)> {
        unsafe {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            self.dupl.AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)?;
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

    /// Copies `texture` into the persistent `last_frame` cache (creating it
    /// on first use, matching `texture`'s description minus the
    /// shared/CPU-access flags the duplication surface carries). Called
    /// after every successfully encoded frame so [`cached_texture`] can
    /// stand in for the next one if `AcquireNextFrame` times out.
    pub fn cache_frame(&mut self, texture: &ID3D11Texture2D) -> Result<()> {
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);

            if self.last_frame.is_none() {
                desc.Usage          = D3D11_USAGE_DEFAULT;
                desc.BindFlags      = D3D11_BIND_SHADER_RESOURCE.0 as u32;
                desc.CPUAccessFlags = 0;
                desc.MiscFlags      = 0;

                let mut cache = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut cache))?;
                self.last_frame = cache;
            }

            let context = self.device.GetImmediateContext()?;
            context.CopyResource(self.last_frame.as_ref().unwrap(), texture);
        }
        Ok(())
    }

    /// The last frame cached via [`cache_frame`], if any — re-submitted to
    /// the encoder on `DXGI_ERROR_WAIT_TIMEOUT` to keep the stream alive
    /// while the desktop is static.
    pub fn cached_texture(&self) -> Option<&ID3D11Texture2D> {
        self.last_frame.as_ref()
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
