wit_bindgen::generate!({
    path: "../../tests/runtime/smoke",
    world_exports: Exports
});

struct Exports;

impl Smoke for Exports {
    fn thunk() {
        test::smoke::imports::thunk();
    }
}
