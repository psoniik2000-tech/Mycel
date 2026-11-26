//! A space's password kept by the OS (Credential Manager / Android Keystore), so the app opens
//! without asking for it.

#[cfg(not(target_os = "android"))]
mod imp {
    use std::path::Path;

    // shortcut: on Linux keyring has no backend enabled here (mock store), so "remember" lasts until exit.
    /// Keyed by the data folder too: another profile (`MYCEL_DIR`) has its own `main`.
    fn entry(root: &Path, id: &str) -> keyring::Result<keyring::Entry> {
        keyring::Entry::new("Mycel", &format!("{id}@{}", root.display()))
    }

    pub fn save(root: &Path, id: &str, pw: &str) -> Result<(), String> {
        entry(root, id)
            .and_then(|e| e.set_password(pw))
            .map_err(|e| e.to_string())
    }

    pub fn load(root: &Path, id: &str) -> Option<String> {
        entry(root, id).ok()?.get_password().ok()
    }

    pub fn forget(root: &Path, id: &str) {
        if let Ok(e) = entry(root, id) {
            let _ = e.delete_credential();
        }
    }
}

/// Android: the password sealed by an AES key that never leaves the Keystore (`Vault.kt`),
/// the ciphertext in `<root>/keys/<id>` (DESIGN §10).
#[cfg(target_os = "android")]
mod imp {
    use jni::JNIEnv;
    use jni::objects::{JByteArray, JClass, JObject, JValue};
    use std::path::{Path, PathBuf};

    fn file(root: &Path, id: &str) -> PathBuf {
        root.join("keys").join(id)
    }

    /// Calls `Vault.<method>(byte[]): byte[]`.
    fn call(
        env: &mut JNIEnv,
        class: &JClass,
        method: &str,
        data: &[u8],
    ) -> jni::errors::Result<Vec<u8>> {
        let arr = env.byte_array_from_slice(data)?;
        let out: JByteArray = env
            .call_static_method(class, method, "([B)[B", &[JValue::Object(&arr)])?
            .l()?
            .into();
        env.convert_byte_array(&out)
    }

    fn vault(method: &str, data: &[u8]) -> Result<Vec<u8>, String> {
        with_class("org.mycel.app.Vault", |env, class, _| {
            call(env, class, method, data)
        })
    }

    /// Runs `f` with an app class and the app's context on any native thread (also `Push`, lib.rs).
    pub fn with_class<T>(
        name: &str,
        f: impl FnOnce(&mut JNIEnv, &JClass, &JObject) -> jni::errors::Result<T>,
    ) -> Result<T, String> {
        let cx = ndk_context::android_context();
        // SAFETY: tauri initialises ndk_context with the live VM and activity before any command runs.
        let vm = unsafe { jni::JavaVM::from_raw(cx.vm().cast()) }.map_err(|e| e.to_string())?;
        let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
        let res = (|| {
            // SAFETY: a global reference owned by ndk_context; JObject does not free it.
            let ctx = unsafe { JObject::from_raw(cx.context().cast()) };
            // App classes are not visible to FindClass on a native thread: go through the app's loader.
            let loader = env
                .call_method(&ctx, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
                .l()?;
            let name = env.new_string(name)?;
            let class: JClass = env
                .call_method(
                    &loader,
                    "loadClass",
                    "(Ljava/lang/String;)Ljava/lang/Class;",
                    &[JValue::Object(&name)],
                )?
                .l()?
                .into();
            f(&mut env, &class, &ctx)
        })();
        // A pending Java exception would crash the next JNI call.
        if res.is_err() {
            let _ = env.exception_clear();
        }
        res.map_err(|e: jni::errors::Error| e.to_string())
    }

    pub fn save(root: &Path, id: &str, pw: &str) -> Result<(), String> {
        let sealed = vault("seal", pw.as_bytes())?;
        let f = file(root, id);
        std::fs::create_dir_all(f.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(f, sealed).map_err(|e| e.to_string())
    }

    pub fn load(root: &Path, id: &str) -> Option<String> {
        let sealed = std::fs::read(file(root, id)).ok()?;
        String::from_utf8(vault("open", &sealed).ok()?).ok()
    }

    pub fn forget(root: &Path, id: &str) {
        let _ = std::fs::remove_file(file(root, id));
    }

    /// `load` on a thread Java called into (a push with no app running): no ndk_context there.
    pub fn load_in(env: &mut JNIEnv, root: &Path, id: &str) -> Option<String> {
        let sealed = std::fs::read(file(root, id)).ok()?;
        let res = env
            .find_class("org/mycel/app/Vault")
            .and_then(|c| call(env, &c, "open", &sealed));
        if res.is_err() {
            let _ = env.exception_clear();
        }
        String::from_utf8(res.ok()?).ok()
    }
}

pub use imp::{forget, load, save};
#[cfg(target_os = "android")]
pub use imp::{load_in, with_class};
