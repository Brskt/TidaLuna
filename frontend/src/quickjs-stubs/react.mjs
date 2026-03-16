/**
 * React stub for rquickjs runtime.
 * Plugins use React only for Settings UI rendered in CEF — these are no-ops.
 */
const noop = () => {};
const identity = (x) => x;
const noopHook = (init) => [typeof init === "function" ? init() : init, noop];

export const createElement = (..._args) => null;
export const cloneElement = (..._args) => null;
export const createContext = (_default) => ({
    Provider: null,
    Consumer: null,
    _currentValue: _default,
});
export const createRef = () => ({ current: null });
export const forwardRef = (render) => render;
export const memo = identity;
export const lazy = () => null;

export const Fragment = Symbol.for("react.fragment");
export const StrictMode = Symbol.for("react.strict_mode");

// Hooks — all no-ops in QuickJS context
export const useState = noopHook;
export const useReducer = (reducer, init) => [init, noop];
export const useEffect = noop;
export const useLayoutEffect = noop;
export const useInsertionEffect = noop;
export const useCallback = identity;
export const useMemo = (factory) => factory();
export const useRef = (init) => ({ current: init ?? null });
export const useContext = (ctx) => ctx._currentValue;
export const useId = () => "";
export const useDeferredValue = identity;
export const useTransition = () => [false, (cb) => cb()];
export const useSyncExternalStore = (sub, getSnapshot) => getSnapshot();
export const useImperativeHandle = noop;
export const useDebugValue = noop;

export class Component {
    constructor(props) {
        this.props = props;
        this.state = {};
    }
    setState() {}
    forceUpdate() {}
    render() { return null; }
}

export class PureComponent extends Component {}

export const Children = {
    map: (_children, _fn) => [],
    forEach: noop,
    count: () => 0,
    only: (c) => c,
    toArray: () => [],
};

export const isValidElement = () => false;
export const version = "19.0.0-quickjs-stub";

const React = {
    createElement, cloneElement, createContext, createRef, forwardRef, memo, lazy,
    Fragment, StrictMode,
    useState, useReducer, useEffect, useLayoutEffect, useInsertionEffect,
    useCallback, useMemo, useRef, useContext, useId, useDeferredValue,
    useTransition, useSyncExternalStore, useImperativeHandle, useDebugValue,
    Component, PureComponent, Children, isValidElement, version,
};
export default React;
