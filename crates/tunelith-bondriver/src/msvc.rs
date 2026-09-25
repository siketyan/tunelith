//! The MSVC run-time type information of IBonDriver2, for the hosts that
//! `dynamic_cast` the object to it, TVTest among them. MSVC finds it through
//! the pointer before the vtable, and compares the types by their decorated
//! names. On x64 its structures refer to one another by offsets from the image
//! base, which it takes to be the address of the locator less the offset the
//! locator gives of itself: the offsets are from the start of [`RTTI`], then.

use std::mem::offset_of;

use crate::{VTABLE, Vtable};

#[repr(C)]
struct Locator {
    /// 1 for the offsets from the image base.
    signature: u32,
    offset: u32,
    constructor_offset: u32,
    type_descriptor: u32,
    hierarchy: u32,
    this: u32,
}

#[repr(C)]
struct Hierarchy {
    signature: u32,
    /// None of multiple or virtual inheritance.
    attributes: u32,
    base_count: u32,
    bases: u32,
}

#[repr(C)]
struct Base {
    type_descriptor: u32,
    contained_bases: u32,
    member_displacement: i32,
    /// -1: not a virtual base.
    vbtable_displacement: i32,
    vbtable_offset: i32,
    /// 0x40: `hierarchy` is there.
    attributes: u32,
    hierarchy: u32,
}

#[repr(C)]
struct TypeDescriptor<const N: usize> {
    /// That of `type_info`, which `dynamic_cast` does not look at.
    vftable: usize,
    spare: usize,
    name: [u8; N],
}

#[repr(C)]
pub struct Rtti {
    locator_ptr: &'static Locator,
    pub vtable: Vtable,
    locator: Locator,
    hierarchy2: Hierarchy,
    bases2: [u32; 2],
    base2: Base,
    base1: Base,
    hierarchy1: Hierarchy,
    bases1: [u32; 1],
    type2: TypeDescriptor<18>,
    type1: TypeDescriptor<17>,
}

const fn at(offset: usize) -> u32 {
    offset as u32
}

const fn base(type_descriptor: usize, contained_bases: u32, hierarchy: usize) -> Base {
    Base {
        type_descriptor: at(type_descriptor),
        contained_bases,
        member_displacement: 0,
        vbtable_displacement: -1,
        vbtable_offset: 0,
        attributes: 0x40,
        hierarchy: at(hierarchy),
    }
}

pub static RTTI: Rtti = Rtti {
    locator_ptr: &RTTI.locator,
    vtable: VTABLE,
    locator: Locator {
        signature: 1,
        offset: 0,
        constructor_offset: 0,
        type_descriptor: at(offset_of!(Rtti, type2)),
        hierarchy: at(offset_of!(Rtti, hierarchy2)),
        this: at(offset_of!(Rtti, locator)),
    },
    hierarchy2: Hierarchy {
        signature: 0,
        attributes: 0,
        base_count: 2,
        bases: at(offset_of!(Rtti, bases2)),
    },
    bases2: [at(offset_of!(Rtti, base2)), at(offset_of!(Rtti, base1))],
    base2: base(offset_of!(Rtti, type2), 1, offset_of!(Rtti, hierarchy2)),
    base1: base(offset_of!(Rtti, type1), 0, offset_of!(Rtti, hierarchy1)),
    hierarchy1: Hierarchy {
        signature: 0,
        attributes: 0,
        base_count: 1,
        bases: at(offset_of!(Rtti, bases1)),
    },
    bases1: [at(offset_of!(Rtti, base1))],
    type2: TypeDescriptor {
        vftable: 0,
        spare: 0,
        name: *b".?AVIBonDriver2@@\0",
    },
    type1: TypeDescriptor {
        vftable: 0,
        spare: 0,
        name: *b".?AVIBonDriver@@\0",
    },
};

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

    use super::*;

    /// Follows the vtable to the name of the complete type as MSVC does.
    #[test]
    fn complete_type() {
        unsafe {
            let vtable = &RTTI.vtable as *const Vtable as *const *const u8;
            let locator = *vtable.sub(1);
            let offset = |at: usize| (locator.add(at) as *const u32).read() as usize;
            let image = locator.sub(offset(offset_of!(Locator, this)));
            let name = image
                .add(offset(offset_of!(Locator, type_descriptor)))
                .add(offset_of!(TypeDescriptor<18>, name));
            assert_eq!(CStr::from_ptr(name.cast()), c".?AVIBonDriver2@@",);
        }
    }
}
