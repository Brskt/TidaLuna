/**
 * react-dom/client stub for rquickjs runtime.
 * UI rendering happens in CEF — these are no-ops.
 */
const noopRoot = {
    render() {},
    unmount() {},
};

export const createRoot = () => noopRoot;
export const hydrateRoot = () => noopRoot;

const ReactDOMClient = { createRoot, hydrateRoot };
export default ReactDOMClient;
