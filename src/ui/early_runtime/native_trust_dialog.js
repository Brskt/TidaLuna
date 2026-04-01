// Fragment 7 — Native plugin trust dialog
// Shows a blocking modal overlay when a native plugin requests access
// to a restricted module (fs, net, child_process, etc.).
// The user must Allow or Deny before the plugin can proceed.

self.__LUNAR_IPC_ON__('native.trust_request', function(pluginName, moduleName, codeHash, manifest) {
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

    // Split "DiscordRPC/discord.native.ts" → plugin="DiscordRPC", file="discord.native.ts"
    var parts = pluginName.split('/');
    var displayPlugin = parts[0] || pluginName;
    var displayFile = parts.length > 1 ? parts.slice(1).join('/') : '';

    var overlay = document.createElement('div');
    overlay.style.cssText = 'position:fixed;top:0;left:0;width:100%;height:100%;background:rgba(0,0,0,0.7);z-index:2147483647;display:flex;align-items:center;justify-content:center;font-family:system-ui,sans-serif';

    var dialog = document.createElement('div');
    dialog.style.cssText = 'background:#1a1a1a;border:1px solid #333;border-radius:8px;padding:24px;max-width:480px;width:90%;color:#fff';

    var title = document.createElement('h2');
    title.style.cssText = 'margin:0 0 16px;font-size:16px';
    title.textContent = 'Plugin Permission Request';

    // Author block (avatar + name)
    var author = manifest && manifest.author;
    var authorBlock = null;
    if (author && author.name) {
        authorBlock = document.createElement('div');
        authorBlock.style.cssText = 'display:flex;align-items:center;gap:10px;margin:0 0 14px;padding:10px 14px;background:#222;border-radius:4px';

        var avatarSrc = author.avatarUrl;
        // Fallback: derive GitHub avatar from author URL if gravatar fails
        if (!avatarSrc && author.url && author.url.indexOf('github.com/') !== -1) {
            var ghUser = author.url.split('github.com/')[1];
            if (ghUser) avatarSrc = 'https://github.com/' + ghUser.split('/')[0] + '.png?size=64';
        }
        if (avatarSrc) {
            var avatar = document.createElement('img');
            avatar.style.cssText = 'width:32px;height:32px;border-radius:50%;flex-shrink:0';
            avatar.onerror = function() {
                // If primary fails, try GitHub avatar from author URL
                if (this.src !== avatarSrc && author.url && author.url.indexOf('github.com/') !== -1) {
                    var ghUser = author.url.split('github.com/')[1];
                    if (ghUser) { this.src = 'https://github.com/' + ghUser.split('/')[0] + '.png?size=64'; return; }
                }
                this.style.display = 'none';
            };
            avatar.src = avatarSrc;
            authorBlock.appendChild(avatar);
        }

        var authorInfo = document.createElement('div');
        var authorName = document.createElement('div');
        authorName.style.cssText = 'font-size:13px;color:#fff;font-weight:600';
        authorName.textContent = author.name;
        authorInfo.appendChild(authorName);

        if (manifest.description) {
            var pluginDesc = document.createElement('div');
            pluginDesc.style.cssText = 'font-size:12px;color:#999;margin-top:2px';
            pluginDesc.textContent = manifest.description;
            authorInfo.appendChild(pluginDesc);
        }

        authorBlock.appendChild(authorInfo);
    }

    // Plugin info block
    var infoBlock = document.createElement('div');
    infoBlock.style.cssText = 'margin:0 0 16px;background:#222;padding:10px 14px;border-radius:4px;font-size:13px;color:#ccc;line-height:1.6';

    var pluginLine = document.createElement('div');
    var pluginLabel = document.createElement('span');
    pluginLabel.style.cssText = 'color:#999';
    pluginLabel.textContent = 'Plugin: ';
    var pluginValue = document.createElement('strong');
    pluginValue.style.cssText = 'color:#fff';
    pluginValue.textContent = displayPlugin;
    pluginLine.appendChild(pluginLabel);
    pluginLine.appendChild(pluginValue);
    infoBlock.appendChild(pluginLine);

    if (displayFile) {
        var fileLine = document.createElement('div');
        var fileLabel = document.createElement('span');
        fileLabel.style.cssText = 'color:#999';
        fileLabel.textContent = 'File: ';
        var fileValue = document.createElement('span');
        fileValue.textContent = displayFile;
        fileLine.appendChild(fileLabel);
        fileLine.appendChild(fileValue);
        infoBlock.appendChild(fileLine);
    }

    var accessLine = document.createElement('div');
    var accessLabel = document.createElement('span');
    accessLabel.style.cssText = 'color:#999';
    accessLabel.textContent = 'Requested access: ';
    var accessValue = document.createElement('strong');
    accessValue.style.cssText = 'color:#eb1e32';
    accessValue.textContent = info.label;
    accessLine.appendChild(accessLabel);
    accessLine.appendChild(accessValue);
    infoBlock.appendChild(accessLine);

    // Permission description — inside the info block, under the access line
    var permDesc = document.createElement('div');
    permDesc.style.cssText = 'margin-top:6px;padding-top:6px;border-top:1px solid #333;color:#aaa;font-size:12px';
    permDesc.textContent = info.desc;
    infoBlock.appendChild(permDesc);

    var warnBlock = document.createElement('div');
    warnBlock.style.cssText = 'margin:0 0 16px;background:#222;padding:10px 14px;border-radius:4px;font-size:12px;color:#999;line-height:1.5';
    var warnText = document.createElement('div');
    warnText.textContent = 'Only allow if you trust this plugin.';
    warnBlock.appendChild(warnText);
    var warnRemember = document.createElement('div');
    warnRemember.style.cssText = 'margin-top:6px';
    warnRemember.textContent = 'This decision will be remembered unless the plugin is:';
    warnBlock.appendChild(warnRemember);
    var warnList = document.createElement('ul');
    warnList.style.cssText = 'margin:4px 0 0 16px;padding:0;list-style:disc';
    var li1 = document.createElement('li');
    li1.textContent = 'Reinstalled';
    var li2 = document.createElement('li');
    li2.textContent = 'Updated';
    warnList.appendChild(li1);
    warnList.appendChild(li2);
    warnBlock.appendChild(warnList);

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
    if (authorBlock) dialog.appendChild(authorBlock);
    dialog.appendChild(infoBlock);
    dialog.appendChild(warnBlock);
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
