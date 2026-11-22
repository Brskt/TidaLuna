// Tests `early-runtime/xhr_pin.js`, which pins XMLHttpRequest for the whole page. Run for real on a
// stand-in `window` and constructor: the pins (non-configurable by design) land on objects this file
// throws away.

import { expect, test } from "bun:test";

import { fragmentSource, runFragment } from "./helpers/early-runtime";

const source = await fragmentSource("xhr_pin");

test("a plugin can neither replace nor wrap the XHR the SDK sends its token with", () => {
	class Stub {
		open() {}
		setRequestHeader() {}
	}
	const nativeOpen = Stub.prototype.open;
	const nativeSetHeader = Stub.prototype.setRequestHeader;
	const window: any = { XMLHttpRequest: Stub };
	runFragment(source, { window, XMLHttpRequest: Stub });
	const spy = () => {};

	window.XMLHttpRequest = class {};
	Stub.prototype.open = spy;
	Stub.prototype.setRequestHeader = spy;

	expect(window.XMLHttpRequest).toBe(Stub);
	expect(Stub.prototype.open).toBe(nativeOpen);
	expect(Stub.prototype.setRequestHeader).toBe(nativeSetHeader);
	expect(() => Object.defineProperty(Stub.prototype, "open", { value: spy })).toThrow(TypeError);
	expect(() => Object.defineProperty(Stub.prototype, "setRequestHeader", { value: spy })).toThrow(TypeError);
	expect(() => Object.defineProperty(window, "XMLHttpRequest", { value: spy })).toThrow(TypeError);
});
