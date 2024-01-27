mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        isyswasfa: "-guest",
        exports: {
            "component:guest/baz": super::Component
        }
    });
}

use {
    async_trait::async_trait,
    bindings::{component::guest::baz, exports::component::guest::baz::Guest as Baz},
};

struct Component;

#[async_trait(?Send)]
impl Baz for Component {
    async fn foo(s: String) -> String {
        format!(
            "{} - exited guest",
            baz::foo(&format!("{s} - entered guest")).await
        )
    }
}
