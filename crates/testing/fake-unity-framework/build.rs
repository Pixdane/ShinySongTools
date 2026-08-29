//! Build script: generate no-op Rust exports for every symbol in
//! `required-exports-0.1.4.txt` except the ones hand-implemented in lib.rs.
//! The export list is pinned to il2cpp-bridge-rs 0.1.4 (see the copied file).

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let export_file = manifest.join("required-exports-0.1.4.txt");
    println!("cargo:rerun-if-changed={}", export_file.display());
    let special: BTreeSet<&str> = [
        "il2cpp_assembly_get_image",
        "il2cpp_class_from_name",
        "il2cpp_class_get_assemblyname",
        "il2cpp_class_get_fields",
        "il2cpp_class_get_image",
        "il2cpp_class_get_interfaces",
        "il2cpp_class_get_methods",
        "il2cpp_class_get_name",
        "il2cpp_class_get_namespace",
        "il2cpp_class_get_nested_types",
        "il2cpp_class_get_parent",
        "il2cpp_class_get_type",
        "il2cpp_class_get_type_token",
        "il2cpp_domain_get",
        "il2cpp_domain_get_assemblies",
        "il2cpp_image_get_class",
        "il2cpp_image_get_class_count",
        "il2cpp_image_get_entry_point",
        "il2cpp_image_get_filename",
        "il2cpp_image_get_name",
        "il2cpp_is_vm_thread",
        "il2cpp_method_get_declaring_type",
        "il2cpp_method_get_flags",
        "il2cpp_method_get_name",
        "il2cpp_method_get_param",
        "il2cpp_method_get_param_count",
        "il2cpp_method_get_param_name",
        "il2cpp_method_get_return_type",
        "il2cpp_method_get_token",
        "il2cpp_method_is_generic",
        "il2cpp_method_is_inflated",
        "il2cpp_method_is_instance",
        "il2cpp_thread_attach",
        "il2cpp_thread_current",
        "il2cpp_thread_detach",
        "il2cpp_type_get_name",
    ]
    .into_iter()
    .collect();
    let exports = fs::read_to_string(&export_file).expect("required export list");
    let mut generated = String::new();
    for (index, name) in exports
        .lines()
        .filter(|name| !special.contains(name))
        .enumerate()
    {
        generated.push_str(&format!(
            "#[unsafe(export_name = \"{name}\")] pub extern \"C\" fn required_stub_{index}() -> *mut std::ffi::c_void {{ std::ptr::null_mut() }}\n"
        ));
    }
    let output = PathBuf::from(env::var("OUT_DIR").expect("out dir")).join("required_stubs.rs");
    fs::write(output, generated).expect("write generated exports");
}
