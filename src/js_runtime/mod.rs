pub mod bridge;
pub mod fetch;
pub mod loader;
pub mod plugin_runtime;
pub mod shims;
pub mod storage;
pub mod transpile;

use rquickjs::{Context, Function, Runtime};

/// Initialize the QuickJS runtime and context.
pub fn init() -> Result<(Runtime, Context), rquickjs::Error> {
    let rt = Runtime::new()?;

    rt.set_memory_limit(64 * 1024 * 1024);
    rt.set_max_stack_size(1024 * 1024);

    let ctx = Context::full(&rt)?;

    ctx.with(|ctx| {
        let globals = ctx.globals();

        let console = rquickjs::Object::new(ctx.clone())?;

        let log = Function::new(ctx.clone(), |msg: String| {
            crate::vprintln!("[JS] {}", msg);
        })?
        .with_name("log")?;

        let warn = Function::new(ctx.clone(), |msg: String| {
            eprintln!("[JS:WARN] {}", msg);
        })?
        .with_name("warn")?;

        let error = Function::new(ctx.clone(), |msg: String| {
            eprintln!("[JS:ERROR] {}", msg);
        })?
        .with_name("error")?;

        console.set("log", log)?;
        console.set("warn", warn)?;
        console.set("error", error)?;
        console.set(
            "info",
            Function::new(ctx.clone(), |msg: String| {
                crate::vprintln!("[JS:INFO] {}", msg);
            })?
            .with_name("info")?,
        )?;

        globals.set("console", console)?;

        Ok::<_, rquickjs::Error>(())
    })?;

    Ok((rt, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_eval() {
        let (_rt, ctx) = init().expect("Failed to init JS runtime");
        ctx.with(|ctx| {
            let result: i32 = ctx.eval("2 + 2").unwrap();
            assert_eq!(result, 4);
        });
    }

    #[test]
    fn test_console_log() {
        let (_rt, ctx) = init().expect("Failed to init JS runtime");
        ctx.with(|ctx| {
            // Should not panic
            let _: () = ctx.eval("console.log('hello from QuickJS')").unwrap();
            let _: () = ctx.eval("console.warn('warning test')").unwrap();
            let _: () = ctx.eval("console.error('error test')").unwrap();
        });
    }

    #[test]
    fn test_globals_exist() {
        let (_rt, ctx) = init().expect("Failed to init JS runtime");
        ctx.with(|ctx| {
            // Standard intrinsics should be available
            let result: bool = ctx.eval("typeof JSON !== 'undefined'").unwrap();
            assert!(result);
            let result: bool = ctx.eval("typeof Promise !== 'undefined'").unwrap();
            assert!(result);
        });
    }
}
