//! Blocking fcntl passthrough must not introduce a single-thread resume window.
//! This structural guard does not prove that a runtime caller entered a wait.
use umbra_platform_macos::abi::TRACED_STUBS;

#[test]
fn fcntl_passthrough_has_no_traced_stub() {
    for &(symbol, number, _) in TRACED_STUBS {
        assert!(
            symbol != "__fcntl" && symbol != "__fcntl_nocancel",
            "blocking fcntl must preserve sibling progress: {symbol}"
        );
        assert!(
            number != 92 && number != 406,
            "fcntl syscall {number} must not acquire a traced return window"
        );
    }
}
