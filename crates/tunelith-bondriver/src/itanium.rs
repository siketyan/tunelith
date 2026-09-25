//! The Itanium C++ ABI run-time type information of IBonDriver2, for the hosts
//! that `dynamic_cast` the object to it, BonDriverProxy_Linux among them. The
//! two words before the vtable give the offset of the object and its
//! `type_info`, whose own vtable is that of the C++ runtime, which compares the
//! types by their mangled names.

use std::ffi::{CStr, c_char};

use crate::{VTABLE, Vtable};

// The C++ runtime of the host, linked for these.
#[cfg_attr(target_os = "linux", link(name = "stdc++"))]
#[cfg_attr(target_vendor = "apple", link(name = "c++"))]
unsafe extern "C" {
    /// `__cxxabiv1::__class_type_info`, for a class without a base.
    #[link_name = "_ZTVN10__cxxabiv117__class_type_infoE"]
    static CLASS_TYPE_INFO: [usize; 0];
    /// `__cxxabiv1::__si_class_type_info`, for a class of a single public base.
    #[link_name = "_ZTVN10__cxxabiv120__si_class_type_infoE"]
    static SI_CLASS_TYPE_INFO: [usize; 0];
}

#[repr(C)]
struct TypeInfo {
    /// Past the offset and the `type_info` of that vtable.
    vtable: *const usize,
    name: *const c_char,
}

#[repr(C)]
struct SiTypeInfo {
    info: TypeInfo,
    base: &'static TypeInfo,
}

// Refers to nothing but constants.
unsafe impl Sync for TypeInfo {}
unsafe impl Sync for SiTypeInfo {}

const fn type_info(vtable: *const [usize; 0], name: &'static CStr) -> TypeInfo {
    TypeInfo {
        vtable: vtable.cast::<usize>().wrapping_add(2),
        name: name.as_ptr(),
    }
}

static IBONDRIVER: TypeInfo = type_info(&raw const CLASS_TYPE_INFO, c"10IBonDriver");

static IBONDRIVER2: SiTypeInfo = SiTypeInfo {
    info: type_info(&raw const SI_CLASS_TYPE_INFO, c"11IBonDriver2"),
    base: &IBONDRIVER,
};

#[repr(C)]
pub struct Rtti {
    offset_to_top: isize,
    type_info: &'static SiTypeInfo,
    pub vtable: Vtable,
}

pub static RTTI: Rtti = Rtti {
    offset_to_top: 0,
    type_info: &IBONDRIVER2,
    vtable: VTABLE,
};

#[cfg(test)]
mod tests {
    use std::ffi::c_void;
    use std::ptr;

    use super::*;

    unsafe extern "C" {
        fn __dynamic_cast(
            object: *const c_void,
            from: *const TypeInfo,
            to: *const TypeInfo,
            hint: isize,
        ) -> *mut c_void;
    }

    /// Casts as a host would, with `type_info`s of its own.
    #[test]
    fn dynamic_cast() {
        let class = |name| type_info(&raw const CLASS_TYPE_INFO, name);
        let host = class(c"10IBonDriver");
        let object = [&RTTI.vtable as *const Vtable];
        let object = object.as_ptr().cast();
        let cast = |to: &SiTypeInfo| unsafe { __dynamic_cast(object, &host, &to.info, -1) };

        let ibondriver2 = SiTypeInfo {
            info: type_info(&raw const SI_CLASS_TYPE_INFO, c"11IBonDriver2"),
            base: &IBONDRIVER,
        };
        assert_eq!(cast(&ibondriver2), object.cast_mut());
        let ibondriver3 = SiTypeInfo {
            info: type_info(&raw const SI_CLASS_TYPE_INFO, c"11IBonDriver3"),
            base: &IBONDRIVER2.info,
        };
        assert_eq!(cast(&ibondriver3), ptr::null_mut());
    }
}
