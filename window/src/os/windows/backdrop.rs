//! A blurred backdrop with an adjustable blur amount, used for
//! `win32_window_background_blur`.
//!
//! The DWM system backdrops (`win32_system_backdrop`) blur by a fixed amount
//! that applications cannot change. Instead, we build the backdrop ourselves
//! with Windows.UI.Composition:
//!
//! ```text
//! DesktopWindowTarget(hwnd)
//!   └ ContainerVisual
//!       ├ SpriteVisual  backdrop -> GaussianBlur(blur)
//!       └ SpriteVisual  CompositionSurface(handle)  <- wgpu swapchain
//! ```
//!
//! The window must be created with `WS_EX_NOREDIRECTIONBITMAP`, and the
//! WebGpu front end presents into the composition surface handle (see
//! `composition_surface_handle`) instead of the HWND.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;
use w::core::{implement, Interface, Result, GUID, HSTRING, PCWSTR};
use w::Foundation::Numerics::Vector2;
use w::Foundation::{IPropertyValue, PropertyValue};
use w::Graphics::Effects::{
    IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectSource_Impl, IGraphicsEffect_Impl,
};
use w::System::DispatcherQueueController;
use w::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND};
use w::Win32::Graphics::DirectComposition::DCompositionCreateSurfaceHandle;
use w::Win32::System::WinRT::Composition::{ICompositorDesktopInterop, ICompositorInterop};
use w::Win32::System::WinRT::Graphics::Direct2D::{
    IGraphicsEffectD2D1Interop, IGraphicsEffectD2D1Interop_Impl, GRAPHICS_EFFECT_PROPERTY_MAPPING,
    GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT,
};
use w::Win32::System::WinRT::{
    CreateDispatcherQueueController, DispatcherQueueOptions, DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT,
};
use w::UI::Composition::Desktop::DesktopWindowTarget;
use w::UI::Composition::{
    CompositionEffectBrush, CompositionEffectSourceParameter, Compositor, ContainerVisual,
    SpriteVisual,
};
use windows_58 as w;

/// CLSID_D2D1GaussianBlur
const CLSID_GAUSSIAN_BLUR: GUID = GUID::from_u128(0x1feb6d69_2fe6_4ac9_8c58_1d7f93e7a6a5);
/// COMPOSITIONOBJECT_READ | COMPOSITIONOBJECT_WRITE
const COMPOSITIONOBJECT_ALL_ACCESS: u32 = 0x0003;

/// A D2D1 Gaussian blur, described in the form Windows.UI.Composition
/// consumes. Properties: 0 = standard deviation, 1 = optimization,
/// 2 = border mode.
#[implement(IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectD2D1Interop)]
struct GaussianBlurEffect {
    source: IGraphicsEffectSource,
    amount: f32,
}

impl IGraphicsEffectSource_Impl for GaussianBlurEffect_Impl {}

impl IGraphicsEffect_Impl for GaussianBlurEffect_Impl {
    fn Name(&self) -> Result<HSTRING> {
        Ok(HSTRING::from("Blur"))
    }
    fn SetName(&self, _name: &HSTRING) -> Result<()> {
        Ok(())
    }
}

impl IGraphicsEffectD2D1Interop_Impl for GaussianBlurEffect_Impl {
    fn GetEffectId(&self) -> Result<GUID> {
        Ok(CLSID_GAUSSIAN_BLUR)
    }
    fn GetNamedPropertyMapping(
        &self,
        name: &PCWSTR,
        index: *mut u32,
        mapping: *mut GRAPHICS_EFFECT_PROPERTY_MAPPING,
    ) -> Result<()> {
        if unsafe { name.to_string() }.map_or(false, |n| n.eq_ignore_ascii_case("BlurAmount")) {
            unsafe {
                *index = 0;
                *mapping = GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT;
            }
            Ok(())
        } else {
            Err(w::Win32::Foundation::E_INVALIDARG.into())
        }
    }
    fn GetPropertyCount(&self) -> Result<u32> {
        Ok(3)
    }
    fn GetProperty(&self, index: u32) -> Result<IPropertyValue> {
        match index {
            0 => PropertyValue::CreateSingle(self.amount)?.cast(),
            // D2D1_GAUSSIANBLUR_OPTIMIZATION_BALANCED
            1 => PropertyValue::CreateUInt32(1)?.cast(),
            // D2D1_BORDER_MODE_HARD
            2 => PropertyValue::CreateUInt32(1)?.cast(),
            _ => Err(w::Win32::Foundation::E_INVALIDARG.into()),
        }
    }
    fn GetSource(&self, index: u32) -> Result<IGraphicsEffectSource> {
        if index == 0 {
            Ok(self.source.clone())
        } else {
            Err(w::Win32::Foundation::E_INVALIDARG.into())
        }
    }
    fn GetSourceCount(&self) -> Result<u32> {
        Ok(1)
    }
}

struct ThreadCompositor {
    // Windows.UI.Composition requires a DispatcherQueue on the thread that
    // owns the compositor; there can only be one per thread.
    _queue: DispatcherQueueController,
    compositor: Compositor,
}

thread_local! {
    static COMPOSITOR: RefCell<Option<ThreadCompositor>> = RefCell::new(None);
}

fn compositor() -> Result<Compositor> {
    COMPOSITOR.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let queue = unsafe {
                CreateDispatcherQueueController(DispatcherQueueOptions {
                    dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
                    threadType: DQTYPE_THREAD_CURRENT,
                    apartmentType: DQTAT_COM_NONE,
                })?
            };
            slot.replace(ThreadCompositor {
                _queue: queue,
                compositor: Compositor::new()?,
            });
        }
        Ok(slot.as_ref().unwrap().compositor.clone())
    })
}

/// hwnd -> composition surface handle, so that the renderer (which only has
/// the raw window handle) can find where to present.
static SURFACE_HANDLES: Mutex<Option<HashMap<isize, isize>>> = Mutex::new(None);

/// Returns the composition surface handle that the GPU front end should
/// present into for this window, if the window uses a blurred backdrop.
pub fn composition_surface_handle(hwnd: isize) -> Option<isize> {
    SURFACE_HANDLES
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|map| map.get(&hwnd).copied())
}

pub(crate) struct BlurredBackdrop {
    hwnd: isize,
    compositor: Compositor,
    _target: DesktopWindowTarget,
    blur_visual: SpriteVisual,
    surface_handle: HANDLE,
    blur: f32,
}

impl BlurredBackdrop {
    pub fn new(hwnd: isize, blur: f32) -> Result<Self> {
        let compositor = compositor()?;
        let target = unsafe {
            compositor
                .cast::<ICompositorDesktopInterop>()?
                .CreateDesktopWindowTarget(HWND(hwnd as _), BOOL(1))?
        };

        let root: ContainerVisual = compositor.CreateContainerVisual()?;
        root.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;

        let blur_visual = compositor.CreateSpriteVisual()?;
        blur_visual.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;

        // The renderer's swapchain is bound to this handle; the composition
        // surface created from it shows whatever is presented.
        let surface_handle =
            unsafe { DCompositionCreateSurfaceHandle(COMPOSITIONOBJECT_ALL_ACCESS, None)? };
        let surface = unsafe {
            compositor
                .cast::<ICompositorInterop>()?
                .CreateCompositionSurfaceForHandle(surface_handle)?
        };
        let content_visual = compositor.CreateSpriteVisual()?;
        content_visual.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
        content_visual.SetBrush(&compositor.CreateSurfaceBrushWithSurface(&surface)?)?;

        let children = root.Children()?;
        children.InsertAtBottom(&blur_visual)?;
        children.InsertAtTop(&content_visual)?;
        target.SetRoot(&root)?;

        let mut backdrop = Self {
            hwnd,
            compositor,
            _target: target,
            blur_visual,
            surface_handle,
            blur: -1.0,
        };
        backdrop.set_blur(blur)?;

        SURFACE_HANDLES
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(hwnd, surface_handle.0 as isize);

        Ok(backdrop)
    }

    /// Changes the blur amount. The amount is baked into the effect
    /// factory, so this builds a new brush.
    pub fn set_blur(&mut self, blur: f32) -> Result<()> {
        let blur = blur.clamp(0.0, 250.0);
        if blur == self.blur {
            return Ok(());
        }
        self.blur = blur;

        let backdrop_param = CompositionEffectSourceParameter::Create(&HSTRING::from("Backdrop"))?;
        let source: IGraphicsEffectSource = backdrop_param.cast()?;
        let effect: IGraphicsEffect = GaussianBlurEffect {
            source,
            amount: blur,
        }
        .into();

        let factory = self.compositor.CreateEffectFactory(&effect)?;
        let brush: CompositionEffectBrush = factory.CreateBrush()?;
        brush.SetSourceParameter(
            &HSTRING::from("Backdrop"),
            &self.compositor.CreateBackdropBrush()?,
        )?;
        self.blur_visual.SetBrush(&brush)?;
        Ok(())
    }
}

impl Drop for BlurredBackdrop {
    fn drop(&mut self) {
        if let Some(map) = SURFACE_HANDLES.lock().unwrap().as_mut() {
            map.remove(&self.hwnd);
        }
        unsafe {
            let _ = CloseHandle(self.surface_handle);
        }
    }
}
