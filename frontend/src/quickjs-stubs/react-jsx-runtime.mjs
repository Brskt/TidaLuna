/**
 * react/jsx-runtime stub for rquickjs runtime.
 * JSX compilation targets this module — no-ops since UI renders in CEF.
 */
export const Fragment = Symbol.for("react.fragment");
export const jsx = (..._args) => null;
export const jsxs = jsx;
export const jsxDEV = jsx;

const jsxRuntime = { Fragment, jsx, jsxs, jsxDEV };
export default jsxRuntime;
