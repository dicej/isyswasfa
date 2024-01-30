mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "round-trip",
        isyswasfa: "-guest",
        exports: {
            "component:test/baz": super::Component
        }
    });
}

use {
    async_trait::async_trait,
    bindings::{
        component::test::baz, exports::component::test::baz::Guest as Baz,
        wasi::clocks::monotonic_clock,
    },
};

struct Component;

#[async_trait(?Send)]
impl Baz for Component {
    async fn foo(s: String) -> String {
        monotonic_clock::subscribe_duration(10_000_000).await;

        format!(
            "{} - exited guest",
            baz::foo(&format!("{s} - entered guest")).await
        )
    }
}
