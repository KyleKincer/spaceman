//! The attributes [`super::observe_window`] reads, fetched in one Accessibility
//! round trip (`AXUIElementCopyMultipleAttributeValues`) instead of one per
//! attribute.

use std::{ffi::c_void, ptr, ptr::NonNull};

use objc2_application_services::{
    AXCopyMultipleAttributeOptions, AXError, AXUIElement, AXValue, AXValueType,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFNull, CFRetained, CFString, CFType, CGPoint, CGSize, ConcreteType,
};

use crate::Rect;

use super::ax::attribute;

#[derive(Clone, Copy, Debug)]
pub(super) enum WindowAttribute {
    Role,
    Subrole,
    Minimized,
    FullScreen,
    Position,
    Size,
    FullScreenButton,
    ZoomButton,
    CloseButton,
    Modal,
    Title,
}

/// Indexed by `WindowAttribute as usize`.
const NAMES: [&str; 11] = [
    "AXRole",
    "AXSubrole",
    "AXMinimized",
    "AXFullScreen",
    "AXPosition",
    "AXSize",
    "AXFullScreenButton",
    "AXZoomButton",
    "AXCloseButton",
    "AXModal",
    "AXTitle",
];

impl WindowAttribute {
    fn name(self) -> &'static str {
        NAMES[self as usize]
    }
}

thread_local! {
    static NAME_ARRAY: CFRetained<CFArray<CFString>> = {
        let names: Vec<_> = NAMES.iter().map(|name| CFString::from_static_str(name)).collect();
        CFArray::from_retained_objects(&names)
    };
    static FRAME_NAME_ARRAY: CFRetained<CFArray<CFString>> = {
        let names = [
            CFString::from_static_str("AXPosition"),
            CFString::from_static_str("AXSize"),
        ];
        CFArray::from_retained_objects(&names)
    };
}

/// Read `AXPosition` and `AXSize` in one Accessibility round trip, falling back
/// to individual attribute reads only if the app rejects the batch call.
pub(super) fn read_frame(element: &AXUIElement) -> Option<Rect> {
    let values = FRAME_NAME_ARRAY.with(|names| copy_multiple(element, names, 2));
    let decode = |index: usize, name: &'static str| -> Option<CFRetained<AXValue>> {
        match &values {
            Values::Batch(batch) => {
                let value = batch.get(index)?;
                if value.downcast_ref::<CFNull>().is_some()
                    || value
                        .downcast_ref::<AXValue>()
                        .and_then(embedded_error)
                        .is_some()
                {
                    return None;
                }
                value.downcast::<AXValue>().ok()
            }
            Values::Failed(_) => None,
            Values::Single => attribute::<AXValue>(element, name).ok(),
        }
    };
    let pos_val = decode(0, "AXPosition")?;
    let size_val = decode(1, "AXSize")?;
    let mut position = CGPoint::ZERO;
    let mut size = CGSize::ZERO;
    // SAFETY: `position` and `size` match `CGPoint` / `CGSize` layouts on the stack.
    let ok = unsafe {
        pos_val.value(
            AXValueType::CGPoint,
            NonNull::from(&mut position).cast::<c_void>(),
        ) && size_val.value(
            AXValueType::CGSize,
            NonNull::from(&mut size).cast::<c_void>(),
        )
    };
    ok.then_some(Rect {
        x: position.x.round() as i32,
        y: position.y.round() as i32,
        width: size.width.round() as i32,
        height: size.height.round() as i32,
    })
}

enum Values {
    /// One value (or `CFNull`, or an `AXValue` of `kAXValueAXErrorType`) per name.
    Batch(CFRetained<CFArray<CFType>>),
    /// The batch call timed out or messaging failed. Every attribute reports
    /// that error, as its own read would have, without waiting out the timeout
    /// again.
    Failed(AXError),
    /// The app rejected the batch call itself; read each attribute on its own.
    Single,
}

pub(super) struct WindowAttributes<'a> {
    element: &'a AXUIElement,
    values: Values,
}

impl<'a> WindowAttributes<'a> {
    pub(super) fn read(element: &'a AXUIElement) -> Self {
        let values = NAME_ARRAY.with(|names| copy_multiple(element, names, NAMES.len()));
        Self { element, values }
    }

    /// The same result [`super::ax::attribute`] gives for that attribute.
    pub(super) fn get<T: ConcreteType>(
        &self,
        attribute_name: WindowAttribute,
    ) -> Result<CFRetained<T>, AXError> {
        let value = match &self.values {
            Values::Batch(values) => values
                .get(attribute_name as usize)
                .ok_or(AXError::NoValue)?,
            Values::Failed(error) => return Err(*error),
            Values::Single => return attribute(self.element, attribute_name.name()),
        };
        if value.downcast_ref::<CFNull>().is_some() {
            return Err(AXError::NoValue);
        }
        if let Some(error) = value.downcast_ref::<AXValue>().and_then(embedded_error) {
            return Err(error);
        }
        value.downcast().map_err(|_| AXError::IllegalArgument)
    }

    /// The same result [`super::ax::optional_bool_attribute`] gives.
    pub(super) fn optional_bool(&self, attribute_name: WindowAttribute) -> Result<bool, AXError> {
        match self.get::<CFBoolean>(attribute_name) {
            Ok(value) => Ok(value.value()),
            Err(AXError::NoValue | AXError::AttributeUnsupported) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// The same result [`super::ax::window_frame`] gives.
    pub(super) fn frame(&self) -> Option<Rect> {
        let position = self.ax_value(
            WindowAttribute::Position,
            AXValueType::CGPoint,
            CGPoint::ZERO,
        )?;
        let size = self.ax_value(WindowAttribute::Size, AXValueType::CGSize, CGSize::ZERO)?;
        Some(Rect {
            x: position.x.round() as i32,
            y: position.y.round() as i32,
            width: size.width.round() as i32,
            height: size.height.round() as i32,
        })
    }

    fn ax_value<T: Copy>(
        &self,
        attribute_name: WindowAttribute,
        kind: AXValueType,
        mut zero: T,
    ) -> Option<T> {
        let value = self.get::<AXValue>(attribute_name).ok()?;
        // SAFETY: `zero` matches the layout expected by `kind` (`CGPoint` or `CGSize`).
        unsafe {
            value
                .value(kind, NonNull::from(&mut zero).cast::<c_void>())
                .then_some(zero)
        }
    }
}

fn copy_multiple(element: &AXUIElement, names: &CFArray<CFString>, expected_len: usize) -> Values {
    let mut raw: *const CFArray = ptr::null();
    // SAFETY: `names` is a valid array of `CFString`s and `raw` is writable stack storage that
    // receives a +1 retained `CFArrayRef` under the Core Foundation Copy rule. Options `0`
    // (`kAXCopyMultipleAttributeOptionNone`) report per-attribute errors inside the array.
    let error = unsafe {
        element.copy_multiple_attribute_values(
            names.as_opaque(),
            AXCopyMultipleAttributeOptions(0),
            NonNull::from(&mut raw),
        )
    };
    match error {
        AXError::Success => {}
        AXError::CannotComplete => return Values::Failed(error),
        _ => return Values::Single,
    }
    let Some(raw) = NonNull::new(raw.cast_mut()) else {
        return Values::Single;
    };
    // SAFETY: `raw` is a +1 retained `CFArrayRef` returned on `AXError::Success`.
    let values = unsafe { CFRetained::from_raw(raw) };
    // SAFETY: Accessibility returns an array of Core Foundation objects, so every element is a
    // `CFType`.
    let values = unsafe { CFRetained::cast_unchecked::<CFArray<CFType>>(values) };
    if values.len() == expected_len {
        Values::Batch(values)
    } else {
        Values::Single
    }
}

fn embedded_error(value: &AXValue) -> Option<AXError> {
    // SAFETY: `value` is a live `AXValue`.
    if unsafe { value.r#type() } != AXValueType::AXError {
        return None;
    }
    let mut error = AXError::Success;
    // SAFETY: `AXError` is a transparent `i32`, the layout `kAXValueAXErrorType` stores, and
    // `error` is writable stack storage.
    let decoded = unsafe { value.value(AXValueType::AXError, NonNull::from(&mut error).cast()) };
    Some(if decoded && error != AXError::Success {
        error
    } else {
        AXError::Failure
    })
}
