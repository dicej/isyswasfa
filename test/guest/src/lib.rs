mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        isywasfa: "-guest",
        exports: {
            world: World
        }
    });

    struct World;

    impl Guest for World {
        fn dummy(_input: PollInput) -> PollOutput {
            unreachable!()
        }
    }
}

use {
    async_trait::async_trait,
    bindings::{
        exports::component::guest::original_interface_async::Guest as OriginalInterfaceAsync,
        isyswasfa::isyswasfa::isyswasfa::{Pending, PollInput, PollOutput, Ready},
        Guest,
    },
};

struct Component;

#[async_trait(?Send)]
impl GuestAsync for ComponentAsync {
    async fn foo(s: String) -> String {
        format!(
            "{} - exited guest",
            isyswasfa_bindings::original_interface_async::foo(&format!("{s} - entered guest"))
                .await
        )
    }
}
