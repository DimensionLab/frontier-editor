// SPDX-License-Identifier: Apache-2.0
//! Prints the JSON Schema of the provenance events the core emits
//! (`EditEvent`, `ApprovalEvent`) and the serialisable view/diff types.
//!
//!     cargo run -p frontier-editor --features schemars --example export_schemas

fn main() {
    #[cfg(feature = "schemars")]
    {
        use frontier_editor::provenance::{ApprovalEvent, EditEvent};
        use frontier_editor::{ChangeSet, ViewState};
        let mut generator =
            schemars::SchemaGenerator::new(schemars::generate::SchemaSettings::draft2020_12());
        let edit = generator.subschema_for::<EditEvent>();
        let approval = generator.subschema_for::<ApprovalEvent>();
        let changeset = generator.subschema_for::<ChangeSet>();
        let view = generator.subschema_for::<ViewState>();
        let defs = generator.take_definitions(true);
        let root = serde_json::json!({
            "description": "Provenance events written by hosts of frontier-editor, plus the ChangeSet and ViewState wire types.",
            "oneOf": [edit, approval],
            "$defs": {
                "EditEvent_root": edit_ref(&changeset),
                "ViewState": view,
            },
        });
        let mut root = root;
        if let serde_json::Value::Object(d) = defs {
            root["$defs"].as_object_mut().expect("defs").extend(d);
        }
        println!("{}", serde_json::to_string_pretty(&root).expect("json"));
    }
    #[cfg(not(feature = "schemars"))]
    eprintln!("build with --features schemars");
}

#[cfg(feature = "schemars")]
fn edit_ref(cs: &schemars::Schema) -> schemars::Schema {
    cs.clone()
}
