mod sizes;

use piccolo::{error::LuaError, AnyCallback, Closure, Error, Executor, Lua, StaticError, Value};
use thiserror::Error;

#[test]
fn error_unwind() -> Result<(), StaticError> {
    let mut lua = Lua::core();

    let executor = lua.try_run(|ctx| {
        let closure = Closure::load(
            ctx,
            &br#"
                function do_error()
                    error('test error')
                end

                do_error()
            "#[..],
        )?;
        Ok(ctx
            .state
            .registry
            .stash(&ctx, Executor::start(ctx, closure.into(), ())))
    })?;

    lua.finish(&executor);
    lua.try_run(|ctx| {
        match ctx.state.registry.fetch(&executor).take_return::<()>(ctx)? {
            Err(Error::Lua(LuaError(Value::String(s)))) => assert!(s == "test error"),
            _ => panic!("wrong error returned"),
        }
        Ok(())
    })
}

#[test]
fn error_tostring() -> Result<(), StaticError> {
    let mut lua = Lua::core();

    #[derive(Debug, Error)]
    #[error("test error")]
    struct TestError;

    let executor = lua.try_run(|ctx| {
        let callback = AnyCallback::from_fn(&ctx, |_, _, _| Err(TestError.into()));
        ctx.state.globals.set(ctx, "callback", callback)?;

        let closure = Closure::load(
            ctx,
            &br#"
                local r, e = pcall(callback)
                assert(not r)
                assert(tostring(e) == "test error")
            "#[..],
        )?;

        Ok(ctx
            .state
            .registry
            .stash(&ctx, Executor::start(ctx, closure.into(), ())))
    })?;

    lua.execute(&executor)
}
