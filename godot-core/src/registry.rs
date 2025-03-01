/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

#![allow(dead_code)] // FIXME

use crate::obj::*;
use crate::private::as_storage;
use crate::storage::InstanceStorage;
use godot_ffi as sys;

use sys::interface_fn;

use crate::bind::GodotExt;
use crate::builtin::meta::ClassName;
use crate::builtin::StringName;
use crate::out;
use std::any::Any;
use std::collections::HashMap;
use std::fmt::{Debug, Formatter, Result as FmtResult};
use std::ptr;

#[derive(Debug)]
pub struct ClassPlugin {
    pub class_name: &'static str,
    pub component: PluginComponent,
}

/// Type-erased function obj, holding a `register_class` function.
#[derive(Copy, Clone)]
pub struct ErasedRegisterFn {
    // Wrapper needed because Debug can't be derived on function pointers with reference parameters, so this won't work:
    // pub type ErasedRegisterFn = fn(&mut dyn std::any::Any);
    // (see https://stackoverflow.com/q/53380040)
    pub raw: fn(&mut dyn Any),
}

impl Debug for ErasedRegisterFn {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "0x{:0>16x}", self.raw as u64)
    }
}

#[derive(Debug, Clone)]
pub enum PluginComponent {
    /// Class definition itself, must always be available
    ClassDef {
        base_class_name: &'static str,

        /// Godot low-level`create` function, wired up to library-generated `init`
        generated_create_fn: Option<
            unsafe extern "C" fn(
                _class_userdata: *mut std::ffi::c_void, //
            ) -> sys::GDNativeObjectPtr,
        >,

        free_fn: unsafe extern "C" fn(
            _class_user_data: *mut std::ffi::c_void,
            instance: sys::GDExtensionClassInstancePtr,
        ),
    },

    /// Collected from `#[godot_api] impl MyClass`
    UserMethodBinds {
        /// Callback to library-generated function which registers functions in the `impl`
        ///
        /// Always present since that's the entire point of this `impl` block.
        generated_register_fn: ErasedRegisterFn,
    },

    /// Collected from `#[godot_api] impl GodotExt for MyClass`
    UserVirtuals {
        /// Callback to user-defined `register_class` function
        user_register_fn: Option<ErasedRegisterFn>,

        /// Godot low-level`create` function, wired up to the user's `init`
        user_create_fn: Option<
            unsafe extern "C" fn(
                _class_userdata: *mut std::ffi::c_void, //
            ) -> sys::GDNativeObjectPtr,
        >,

        /// User-defined `to_string` function
        user_to_string_fn: Option<
            unsafe extern "C" fn(
                p_instance: sys::GDExtensionClassInstancePtr,
                r_is_valid: *mut sys::GDNativeBool,
                p_out: sys::GDNativeStringPtr,
            ),
        >,

        /// Callback for other virtuals
        get_virtual_fn: unsafe extern "C" fn(
            p_userdata: *mut std::os::raw::c_void,
            p_name: sys::GDNativeStringNamePtr,
        ) -> sys::GDNativeExtensionClassCallVirtual,
    },
}

// ----------------------------------------------------------------------------------------------------------------------------------------------

#[derive(Debug)]
struct ClassRegistrationInfo {
    class_name: ClassName,
    parent_class_name: Option<ClassName>,
    generated_register_fn: Option<ErasedRegisterFn>,
    user_register_fn: Option<ErasedRegisterFn>,
    godot_params: sys::GDNativeExtensionClassCreationInfo,
}

pub fn register_class<T: GodotExt + cap::GodotInit + cap::ImplementsGodotExt>() {
    // TODO: provide overloads with only some trait impls

    out!("Manually register class {}", std::any::type_name::<T>());
    let class_name = ClassName::new::<T>();

    let godot_params = sys::GDNativeExtensionClassCreationInfo {
        to_string_func: Some(callbacks::to_string::<T>),
        reference_func: Some(callbacks::reference::<T>),
        unreference_func: Some(callbacks::unreference::<T>),
        create_instance_func: Some(callbacks::create::<T>),
        free_instance_func: Some(callbacks::free::<T>),
        get_virtual_func: Some(callbacks::get_virtual::<T>),
        get_rid_func: None,
        class_userdata: ptr::null_mut(), // will be passed to create fn, but global per class
        ..default_creation_info()
    };

    register_class_raw(ClassRegistrationInfo {
        class_name,
        parent_class_name: Some(ClassName::new::<T::Base>()),
        generated_register_fn: None,
        user_register_fn: Some(ErasedRegisterFn {
            raw: callbacks::register_class_by_builder::<T>,
        }),
        godot_params,
    });
}

/// Lets Godot know about all classes that have self-registered through the plugin system.
pub fn auto_register_classes() {
    out!("Auto-register classes...");

    // Note: many errors are already caught by the compiler, before this runtime validation even takes place:
    // * missing #[derive(GodotClass)] or impl GodotClass for T
    // * duplicate impl GodotInit for T
    //

    let mut map = HashMap::<ClassName, ClassRegistrationInfo>::new();

    crate::private::iterate_plugins(|elem: &ClassPlugin| {
        //out!("* Plugin: {elem:#?}");

        let name = ClassName::from_static(elem.class_name);
        let class_info = map
            .entry(name.clone())
            .or_insert_with(|| default_registration_info(name));

        fill_class_info(elem.component.clone(), class_info);
    });

    //out!("Class-map: {map:#?}");

    for info in map.into_values() {
        out!("Register class:   {}", info.class_name);
        register_class_raw(info);
    }

    out!("All classes auto-registered.");
}

fn fill_class_info(component: PluginComponent, c: &mut ClassRegistrationInfo) {
    // out!("|   reg (before):    {c:?}");
    // out!("|   comp:            {component:?}");
    match component {
        PluginComponent::ClassDef {
            base_class_name,
            generated_create_fn,
            free_fn,
        } => {
            c.parent_class_name = Some(ClassName::from_static(base_class_name));
            fill_into(
                &mut c.godot_params.create_instance_func,
                generated_create_fn,
            );
            c.godot_params.free_instance_func = Some(free_fn);
        }

        PluginComponent::UserMethodBinds {
            generated_register_fn,
        } => {
            c.generated_register_fn = Some(generated_register_fn);
        }

        PluginComponent::UserVirtuals {
            user_register_fn,
            user_create_fn,
            user_to_string_fn,
            get_virtual_fn,
        } => {
            c.user_register_fn = user_register_fn;
            fill_into(&mut c.godot_params.create_instance_func, user_create_fn);
            c.godot_params.to_string_func = user_to_string_fn;
            c.godot_params.get_virtual_func = Some(get_virtual_fn);
        }
    }
    // out!("|   reg (after):     {c:?}");
    // out!();
}

fn fill_into<T>(dst: &mut Option<T>, src: Option<T>) {
    match (dst, src) {
        (dst @ None, src) => *dst = src,
        (Some(_), Some(_)) => panic!("option already filled"),
        (Some(_), None) => { /* do nothing */ }
    }
}

fn register_class_raw(info: ClassRegistrationInfo) {
    // First register class...

    let class_name = info.class_name;
    let parent_class_name = info
        .parent_class_name
        .expect("class defined (parent_class_name)");

    unsafe {
        interface_fn!(classdb_register_extension_class)(
            sys::get_library(),
            class_name.string_sys(),
            parent_class_name.string_sys(),
            ptr::addr_of!(info.godot_params),
        );
    }

    // std::mem::forget(class_name);
    // std::mem::forget(parent_class_name);

    // ...then custom symbols

    //let mut class_builder = crate::builder::ClassBuilder::<?>::new();
    let mut class_builder = 0; // TODO dummy argument; see callbacks

    // First call generated (proc-macro) registration function, then user-defined one.
    // This mimics the intuition that proc-macros are running "before" normal runtime code.
    if let Some(register_fn) = info.generated_register_fn {
        (register_fn.raw)(&mut class_builder);
    }
    if let Some(register_fn) = info.user_register_fn {
        (register_fn.raw)(&mut class_builder);
    }
}

// Re-exported to crate::private
pub mod callbacks {
    use super::*;
    use crate::bind::GodotExt;
    use crate::builder::ClassBuilder;
    use crate::obj::Base;

    pub unsafe extern "C" fn create<T: cap::GodotInit>(
        _class_userdata: *mut std::ffi::c_void,
    ) -> sys::GDNativeObjectPtr {
        create_custom(T::__godot_init)
    }

    pub(crate) fn create_custom<T, F>(make_user_instance: F) -> sys::GDNativeObjectPtr
    where
        T: GodotClass,
        F: FnOnce(Base<T::Base>) -> T,
    {
        let class_name = ClassName::new::<T>();
        let base_class_name = ClassName::new::<T::Base>();

        //out!("create callback: {}", class_name.backing);

        let base_ptr =
            unsafe { interface_fn!(classdb_construct_object)(base_class_name.string_sys()) };
        let base = unsafe { Base::from_sys(base_ptr) };

        let user_instance = make_user_instance(base);
        let instance = InstanceStorage::<T>::construct(user_instance);
        let instance_ptr = instance.into_raw();
        let instance_ptr = instance_ptr as *mut std::ffi::c_void; // TODO GDExtensionClassInstancePtr

        let binding_data_callbacks = crate::storage::nop_instance_callbacks();
        unsafe {
            interface_fn!(object_set_instance)(base_ptr, class_name.string_sys(), instance_ptr);
            interface_fn!(object_set_instance_binding)(
                base_ptr,
                sys::get_library(),
                instance_ptr,
                &binding_data_callbacks,
            );
        }

        // std::mem::forget(class_name);
        // std::mem::forget(base_class_name);
        base_ptr
    }

    pub unsafe extern "C" fn free<T: GodotClass>(
        _class_user_data: *mut std::ffi::c_void,
        instance: sys::GDExtensionClassInstancePtr,
    ) {
        let storage = as_storage::<T>(instance);
        storage.mark_destroyed_by_godot();
        let _drop = Box::from_raw(storage as *mut InstanceStorage<_>);
    }

    pub unsafe extern "C" fn get_virtual<T: cap::ImplementsGodotExt>(
        _class_user_data: *mut std::ffi::c_void,
        name: sys::GDNativeStringNamePtr,
    ) -> sys::GDNativeExtensionClassCallVirtual {
        // This string is not ours, so we cannot call the destructor on it.
        let borrowed_string = StringName::from_string_sys(name);
        let method_name = borrowed_string.to_string();
        std::mem::forget(borrowed_string);

        T::__virtual_call(method_name.as_str())
    }

    pub unsafe extern "C" fn to_string<T: GodotExt>(
        instance: sys::GDExtensionClassInstancePtr,
        _is_valid: *mut sys::GDNativeBool,
        out_string: sys::GDNativeStringPtr,
    ) {
        // Note: to_string currently always succeeds, as it is only provided for classes that have a working implementation.
        // is_valid output parameter thus not needed.

        let storage = as_storage::<T>(instance);
        let instance = storage.get();
        let string = <T as GodotExt>::to_string(&*instance);

        // Transfer ownership to Godot, disable destructor
        string.write_string_sys(out_string);
        std::mem::forget(string);
    }

    pub unsafe extern "C" fn reference<T: GodotClass>(instance: sys::GDExtensionClassInstancePtr) {
        let storage = as_storage::<T>(instance);
        storage.on_inc_ref();
    }

    pub unsafe extern "C" fn unreference<T: GodotClass>(
        instance: sys::GDExtensionClassInstancePtr,
    ) {
        let storage = as_storage::<T>(instance);
        storage.on_dec_ref();
    }

    // Safe, higher-level methods

    /// Abstracts the `GodotInit` away, for contexts where this trait bound is not statically available
    pub fn erased_init<T: cap::GodotInit>(base: Box<dyn Any>) -> Box<dyn Any> {
        let concrete = base
            .downcast::<Base<<T as GodotClass>::Base>>()
            .expect("erased_init: bad type erasure");
        let extracted: Base<_> = sys::unbox(concrete);

        let instance = T::__godot_init(extracted);
        Box::new(instance)
    }

    pub fn register_class_by_builder<T: GodotExt>(_class_builder: &mut dyn Any) {
        // TODO use actual argument, once class builder carries state
        // let class_builder = class_builder
        //     .downcast_mut::<ClassBuilder<T>>()
        //     .expect("bad type erasure");

        let mut class_builder = ClassBuilder::new();
        T::register_class(&mut class_builder);
    }

    pub fn register_user_binds<T: cap::ImplementsGodotApi>(_class_builder: &mut dyn Any) {
        // let class_builder = class_builder
        //     .downcast_mut::<ClassBuilder<T>>()
        //     .expect("bad type erasure");

        //T::register_methods(class_builder);
        T::__register_methods();
    }
}

// Substitute for Default impl
// Yes, bindgen can implement Default, but only for _all_ types (with single exceptions).
// For FFI types, it's better to have explicit initialization in the general case though.
fn default_registration_info(class_name: ClassName) -> ClassRegistrationInfo {
    ClassRegistrationInfo {
        class_name,
        parent_class_name: None,
        generated_register_fn: None,
        user_register_fn: None,
        godot_params: default_creation_info(),
    }
}

fn default_creation_info() -> sys::GDNativeExtensionClassCreationInfo {
    sys::GDNativeExtensionClassCreationInfo {
        is_abstract: false as u8,
        is_virtual: false as u8,
        set_func: None,
        get_func: None,
        get_property_list_func: None,
        free_property_list_func: None,
        property_can_revert_func: None,
        property_get_revert_func: None,
        notification_func: None,
        to_string_func: None,
        reference_func: None,
        unreference_func: None,
        create_instance_func: None,
        free_instance_func: None,
        get_virtual_func: None,
        get_rid_func: None,
        class_userdata: ptr::null_mut(),
    }
}
