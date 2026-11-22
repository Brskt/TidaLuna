// XHR pinned to the native functions before any plugin runs
// The SDK's requests carry its bearer; a plugin must neither read nor rewrite their headers.
// Instance-level patches (xhr.setRequestHeader = ...) are not blocked, but need a reference to
// TIDAL's own XHR instances.

var _xhrOpen = XMLHttpRequest.prototype.open;
var _xhrSetHeader = XMLHttpRequest.prototype.setRequestHeader;

// Accessor descriptors: the getter always returns the native function, the setter is a no-op (TIDAL's
// strict-mode libs may assign without a TypeError), configurable:false refuses Object.defineProperty.
// The constructor on window is locked the same way; no subclass instruments instances first.
var _XHR = XMLHttpRequest;
Object.defineProperty(window, 'XMLHttpRequest', {
    get: function() { return _XHR; },
    set: function() {},
    enumerable: true,
    configurable: false
});
Object.defineProperty(XMLHttpRequest.prototype, 'open', {
    get: function() { return _xhrOpen; },
    set: function() {},
    enumerable: true,
    configurable: false
});
Object.defineProperty(XMLHttpRequest.prototype, 'setRequestHeader', {
    get: function() { return _xhrSetHeader; },
    set: function() {},
    enumerable: true,
    configurable: false
});
