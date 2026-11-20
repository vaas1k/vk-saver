#![no_main]
use libfuzzer_sys::fuzz_target;
use vk_engine::export::search::search;
use vk_engine::testing::tmpdir;

// Fuzz the message-search parser over arbitrary page bytes: cp1251/UTF-8 decode,
// tag and script/style stripping, entity unescape, lowercase matching and snippet
// slicing. A panic or UB here is a bug; any Stop/empty result is fine.
fuzz_target!(|data: &[u8]| {
    let dir = tmpdir("fuzz-search");
    let chat = dir.join("messages").join("1");
    if std::fs::create_dir_all(&chat).is_err() {
        return;
    }
    let _ = std::fs::write(chat.join("messages0.html"), data);
    let _ = search(&dir, "привет", 50);
    let _ = std::fs::remove_dir_all(&dir);
});
