// Fragment 7 — Native plugin trust dialog
// Shows a blocking modal overlay when a native plugin requests access
// to a restricted module (fs, net, child_process, etc.).
// The user must Allow or Deny before the plugin can proceed.

self.__LUNAR_IPC_ON__('native.trust_request', function(pluginName, moduleName, codeHash) {
    var MODULE_INFO = {
        'fs':             { label: 'Filesystem',       desc: 'Read, write, and delete files on your computer.' },
        'fs/promises':    { label: 'Filesystem',       desc: 'Read, write, and delete files on your computer.' },
        'child_process':  { label: 'Process Spawning', desc: 'Run programs and shell commands on your computer.' },
        'worker_threads': { label: 'Worker Threads',   desc: 'Run JavaScript code in parallel background threads.' },
        'cluster':        { label: 'Cluster',          desc: 'Spawn multiple copies of this process.' },
        'os':             { label: 'System Info',      desc: 'Read system details: hostname, memory, CPU, user info.' },
        'vm':             { label: 'Code Execution',   desc: 'Evaluate arbitrary JavaScript code in a new context.' },
        'v8':             { label: 'V8 Engine',        desc: 'Access low-level JavaScript engine internals.' },
        'inspector':      { label: 'Debugger',         desc: 'Attach a debugger to inspect and control this process.' }
    };

    var info = MODULE_INFO[moduleName] || { label: moduleName, desc: 'Access the ' + moduleName + ' module.' };
    var label = info.label;
    var description = info.desc;

    var overlay = document.createElement('div');
    overlay.style.cssText = 'position:fixed;top:0;left:0;width:100%;height:100%;background:rgba(0,0,0,0.7);z-index:2147483647;display:flex;align-items:center;justify-content:center;font-family:system-ui,sans-serif';

    var dialog = document.createElement('div');
    dialog.style.cssText = 'background:#1a1a1a;border:1px solid #333;border-radius:8px;padding:24px;max-width:480px;width:90%;color:#fff';

    var title = document.createElement('h2');
    title.style.cssText = 'margin:0 0 12px;font-size:16px';
    title.textContent = 'Plugin Permission Request';

    var desc = document.createElement('p');
    desc.style.cssText = 'margin:0 0 16px;color:#ccc;font-size:14px';
    var pluginBold = document.createElement('strong');
    pluginBold.textContent = pluginName;
    var labelBold = document.createElement('strong');
    labelBold.textContent = label;
    desc.appendChild(document.createTextNode('Plugin '));
    desc.appendChild(pluginBold);
    desc.appendChild(document.createTextNode(' is requesting access to '));
    desc.appendChild(labelBold);
    desc.appendChild(document.createTextNode('.'));

    var detail = document.createElement('p');
    detail.style.cssText = 'margin:0 0 12px;color:#aaa;font-size:13px;background:#222;padding:8px 12px;border-radius:4px';
    detail.textContent = description;

    var warn = document.createElement('p');
    warn.style.cssText = 'margin:0 0 20px;color:#999;font-size:12px';
    warn.textContent = 'Only allow if you trust this plugin. This decision will be remembered.';

    var actions = document.createElement('div');
    actions.style.cssText = 'display:flex;gap:8px;justify-content:flex-end';

    var denyBtn = document.createElement('button');
    denyBtn.style.cssText = 'padding:8px 16px;background:#333;border:none;border-radius:4px;color:#fff;cursor:pointer;font-size:13px';
    denyBtn.textContent = 'Deny';

    var allowBtn = document.createElement('button');
    allowBtn.style.cssText = 'padding:8px 16px;background:#eb1e32;border:none;border-radius:4px;color:#fff;cursor:pointer;font-size:13px';
    allowBtn.textContent = 'Allow';

    actions.appendChild(denyBtn);
    actions.appendChild(allowBtn);
    dialog.appendChild(title);
    dialog.appendChild(desc);
    dialog.appendChild(detail);
    dialog.appendChild(warn);
    dialog.appendChild(actions);
    overlay.appendChild(dialog);
    document.body.appendChild(overlay);

    function respond(granted) {
        document.body.removeChild(overlay);
        invokeIpc('__Luna.native_trust_response', pluginName, moduleName, codeHash, granted);
    }

    denyBtn.onclick = function() { respond(false); };
    allowBtn.onclick = function() { respond(true); };
});
