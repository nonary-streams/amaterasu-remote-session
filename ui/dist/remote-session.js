(function(){
"use strict";
var RemoteSessionUi;
(function (RemoteSessionUi) {
    RemoteSessionUi.booleanSettings = [
        'show_remote_input',
        'show_remote_monitor',
        'remote_monitor_mute_audio',
        'remote_monitor_disconnect_on_stream_end',
        'remote_monitor_disconnect_on_client_disconnect',
        'remote_monitor_terminate_on_first_request',
        'remote_monitor_confirm_app_replacement',
    ];
    function object(value) {
        return value !== null && typeof value === 'object' && !Array.isArray(value)
            ? value
            : null;
    }
    function parseSettings(value) {
        const source = object(value);
        if (!source)
            throw new Error('Remote session settings are invalid.');
        const result = {};
        for (const name of RemoteSessionUi.booleanSettings) {
            if (typeof source[name] !== 'boolean') {
                throw new Error('Remote session settings contain an invalid switch.');
            }
            result[name] = source[name];
        }
        if (typeof source.max_clients !== 'number'
            || !Number.isSafeInteger(source.max_clients)
            || source.max_clients < 1 || source.max_clients > 32) {
            throw new Error('The client limit must be a whole number from 1 to 32.');
        }
        result.max_clients = source.max_clients;
        return result;
    }
    RemoteSessionUi.parseSettings = parseSettings;
    function settingsEqual(left, right) {
        return RemoteSessionUi.booleanSettings.every(name => left[name] === right[name])
            && left.max_clients === right.max_clients;
    }
    RemoteSessionUi.settingsEqual = settingsEqual;
    function requestError(value) {
        const reply = object(value);
        if (reply?.ok !== false)
            return null;
        const message = reply.code === 'revision_conflict'
            ? 'Settings changed in another editor. Refresh saved settings to keep your draft and try again.'
            : typeof reply.message === 'string' ? reply.message : 'The settings request was rejected.';
        return Object.assign(new Error(message), { code: reply.code });
    }
    RemoteSessionUi.requestError = requestError;
    function parseReply(value) {
        const error = requestError(value);
        if (error)
            throw error;
        const reply = object(value);
        if (reply?.ok !== true)
            throw new Error('The settings service did not confirm success.');
        if (typeof reply.revision !== 'number' || !Number.isSafeInteger(reply.revision)
            || reply.revision < 0) {
            throw new Error('The settings revision cannot be represented safely in this editor.');
        }
        return { ok: true, revision: reply.revision, settings: parseSettings(reply.settings) };
    }
    RemoteSessionUi.parseReply = parseReply;
    class SettingsEditor {
        api;
        changed;
        saved = null;
        draft = null;
        busy = false;
        message = '';
        error = false;
        confirmationRequired = false;
        disposed = false;
        generation = 0;
        constructor(api, changed) {
            this.api = api;
            this.changed = changed;
        }
        dirty() {
            return this.saved !== null && this.draft !== null
                && !settingsEqual(this.saved.settings, this.draft);
        }
        valid() {
            try {
                parseSettings(this.draft);
                return this.draft !== null;
            }
            catch {
                return false;
            }
        }
        edit(name, value) {
            if (this.disposed || this.busy || this.draft === null)
                return;
            this.draft = { ...this.draft, [name]: value };
            this.message = '';
            this.error = false;
            this.changed();
        }
        async refresh(discardDraft = false) {
            if (this.disposed || this.busy)
                return;
            const generation = ++this.generation;
            this.busy = true;
            this.message = 'Reading saved settings…';
            this.error = false;
            this.changed();
            try {
                const current = parseReply(await this.api.call('preferences'));
                if (!this.active(generation))
                    return;
                const previous = this.saved;
                const draft = this.draft;
                const refreshed = { ...current.settings };
                if (!discardDraft && previous !== null && draft !== null) {
                    for (const name of RemoteSessionUi.booleanSettings) {
                        if (draft[name] !== previous.settings[name])
                            refreshed[name] = draft[name];
                    }
                    if (draft.max_clients !== previous.settings.max_clients) {
                        refreshed.max_clients = draft.max_clients;
                    }
                }
                this.saved = current;
                this.draft = refreshed;
                this.confirmationRequired = false;
                this.message = this.dirty()
                    ? 'Saved settings refreshed. Your unsaved changes are kept.'
                    : 'Saved settings are up to date.';
            }
            catch (error) {
                if (!this.active(generation))
                    return;
                this.message = errorMessage(error);
                this.error = true;
            }
            finally {
                if (this.active(generation)) {
                    this.busy = false;
                    this.changed();
                }
            }
        }
        async save() {
            if (this.disposed || this.busy || !this.saved || !this.draft
                || !this.dirty() || this.confirmationRequired)
                return;
            let requested;
            try {
                requested = parseSettings(this.draft);
            }
            catch (error) {
                this.message = errorMessage(error);
                this.error = true;
                this.changed();
                return;
            }
            const revision = this.saved.revision;
            if (!Number.isSafeInteger(revision + 1)) {
                this.message = 'The next settings revision cannot be represented safely in this editor.';
                this.error = true;
                this.changed();
                return;
            }
            const generation = ++this.generation;
            this.busy = true;
            this.message = 'Saving settings…';
            this.error = false;
            this.changed();
            let writeRejected = false;
            try {
                const response = await this.api.call('preferences_update', {
                    settings: requested, expected_revision: revision,
                });
                if (!this.active(generation))
                    return;
                const rejection = requestError(response);
                writeRejected = rejection !== null;
                if (rejection)
                    throw rejection;
                const updated = parseReply(response);
                if (updated.revision !== revision + 1 || !settingsEqual(updated.settings, requested)) {
                    throw new Error('The update did not confirm the requested settings. Refresh saved settings to check its outcome.');
                }
                const readback = parseReply(await this.api.call('preferences'));
                if (!this.active(generation))
                    return;
                if (readback.revision !== updated.revision || !settingsEqual(readback.settings, requested)) {
                    throw new Error('Settings changed before the save could be confirmed. Refresh saved settings to check its outcome.');
                }
                this.saved = readback;
                this.draft = { ...readback.settings };
                this.message = 'Settings saved and confirmed.';
                this.confirmationRequired = false;
            }
            catch (error) {
                if (!this.active(generation))
                    return;
                this.message = errorMessage(error);
                this.error = true;
                this.confirmationRequired = !writeRejected;
                if (this.confirmationRequired) {
                    this.message += ' Refresh saved settings before saving again; your draft is kept.';
                }
            }
            finally {
                if (this.active(generation)) {
                    this.busy = false;
                    this.changed();
                }
            }
        }
        dispose() {
            this.disposed = true;
            this.generation += 1;
        }
        active(generation) {
            return !this.disposed && this.generation === generation;
        }
    }
    RemoteSessionUi.SettingsEditor = SettingsEditor;
    function errorMessage(error) {
        return error instanceof Error ? error.message : 'The settings request failed.';
    }
    RemoteSessionUi.errorMessage = errorMessage;
})(RemoteSessionUi || (RemoteSessionUi = {}));
var RemoteSessionUi;
(function (RemoteSessionUi) {
    const descriptions = {
        show_remote_input: {
            label: 'Show Remote Input',
            help: 'Add a client tile for sending keyboard, mouse, and controller input without capturing the host display or audio.',
            group: 'tiles',
        },
        show_remote_monitor: {
            label: 'Show Remote Monitor',
            help: 'Add a client tile for streaming a display retained for that paired client.',
            group: 'tiles',
        },
        remote_monitor_mute_audio: {
            label: 'Mute Remote Monitor audio',
            help: 'Stop sending audio to Remote Monitor clients. Host playback stays enabled.',
            group: 'monitor',
        },
        remote_monitor_disconnect_on_stream_end: {
            label: 'Release monitor when its stream ends',
            help: 'Remove this client’s retained monitor when its stream ends. When off, the client can resume its monitor later.',
            group: 'monitor',
        },
        remote_monitor_disconnect_on_client_disconnect: {
            label: 'Release monitor when its client disconnects',
            help: 'Remove the retained monitor if its paired client loses its streaming connection.',
            group: 'monitor',
        },
        remote_monitor_terminate_on_first_request: {
            label: 'Allow one-request termination by another client',
            help: 'Let another authorized client terminate the active game immediately. When off, that client must select Terminate again within 60 seconds.',
            group: 'safety',
        },
        remote_monitor_confirm_app_replacement: {
            label: 'Confirm replacement of an active game',
            help: 'Require another launch of the same app before replacing the active game. Remote Input and Remote Monitor stay connected.',
            group: 'safety',
        },
    };
    function text(tag, content) {
        const node = document.createElement(tag);
        node.textContent = content;
        return node;
    }
    function mount(container, sdk) {
        const root = document.createElement('section');
        root.className = 'remote-session-settings';
        const heading = text('h2', 'Remote Monitor and Remote Input');
        const introduction = text('p', 'Choose which controls paired clients see and when their monitors disconnect.');
        const notice = text('p', 'Loading settings…');
        notice.className = 'remote-session-notice';
        notice.setAttribute('role', 'status');
        notice.setAttribute('aria-live', 'polite');
        root.append(heading, introduction, notice);
        container.append(root);
        const ui = sdk.ui;
        if (!ui || ui.version !== 1) {
            notice.textContent = 'Remote sessions requires the shared controls UI SDK.';
            return () => root.remove();
        }
        const cards = new Map();
        const fields = new Map();
        const actions = [];
        let limit = null;
        let save = null;
        let refresh = null;
        let discard = null;
        let disposed = false;
        let initialized = false;
        const canRead = sdk.canCall?.('preferences') !== false;
        const canWrite = sdk.canCall?.('preferences_update') === true;
        const editor = new RemoteSessionUi.SettingsEditor(sdk, render);
        const dispose = () => {
            if (disposed)
                return;
            disposed = true;
            editor.dispose();
            for (const field of fields.values())
                field.dispose();
            limit?.dispose();
            for (const action of actions)
                action.dispose();
            for (const card of cards.values())
                card.dispose();
            sdk.signal.removeEventListener('abort', dispose);
            root.remove();
        };
        sdk.signal.addEventListener('abort', dispose, { once: true });
        if (sdk.signal.aborted) {
            dispose();
            return dispose;
        }
        for (const [id, label] of [
            ['tiles', 'Client tiles'], ['monitor', 'Monitor lifetime and audio'], ['safety', 'Game controls'],
        ]) {
            const card = ui.card({ label: label });
            card.content.classList.add('remote-session-fields');
            cards.set(id, card);
            root.append(card.element);
        }
        const buttons = document.createElement('div');
        buttons.className = 'remote-session-actions';
        save = ui.button({ label: 'Save remote session settings', variant: 'primary', disabled: true, onClick: () => editor.save() });
        refresh = ui.button({ label: 'Refresh saved settings', disabled: !canRead, onClick: () => editor.refresh() });
        discard = ui.button({ label: 'Discard changes', disabled: true, onClick: () => editor.refresh(true) });
        actions.push(save, refresh, discard);
        buttons.append(...actions.map(action => action.element));
        root.append(buttons);
        if (!canWrite) {
            const readonly = text('p', 'These settings are read-only for your account.');
            readonly.className = 'remote-session-readonly';
            root.append(readonly);
        }
        function initialize() {
            if (!ui || !editor.draft || initialized)
                return;
            initialized = true;
            for (const name of RemoteSessionUi.booleanSettings) {
                const description = descriptions[name];
                const field = ui.toggle({
                    label: description.label, help: description.help, value: editor.draft[name],
                    disabled: !canWrite,
                    onChange: value => editor.edit(name, value),
                });
                fields.set(name, field);
                cards.get(description.group).append(field.element);
            }
            limit = ui.numberField({
                label: 'Maximum retained clients', value: editor.draft.max_clients,
                minimum: 1, maximum: 32, step: 1, required: true, disabled: !canWrite,
                help: 'Limit the paired clients with retained Remote Monitor or Remote Input sessions. Use a whole number from 1 to 32.',
                onChange: value => editor.edit('max_clients', value),
            });
            cards.get('monitor').append(limit.element);
        }
        function render() {
            if (disposed || sdk.signal.aborted)
                return;
            initialize();
            if (editor.draft) {
                for (const [name, field] of fields) {
                    if (field.value() !== editor.draft[name])
                        field.setValue(editor.draft[name]);
                    field.setState({ disabled: editor.busy || !canWrite });
                }
                if (limit) {
                    if (!Object.is(limit.value(), editor.draft.max_clients))
                        limit.setValue(editor.draft.max_clients);
                    limit.setState({ disabled: editor.busy || !canWrite,
                        error: editor.valid() ? null : 'Enter a whole number from 1 to 32.' });
                }
            }
            save?.setState({ disabled: !canWrite || editor.busy || !editor.dirty()
                    || !editor.valid() || editor.confirmationRequired, busy: editor.busy });
            refresh?.setState({ disabled: editor.busy || !canRead });
            discard?.setState({ disabled: !canWrite || editor.busy || !editor.dirty() });
            notice.textContent = editor.message;
            notice.dataset.error = String(editor.error);
            sdk.setDirty?.(editor.dirty());
            sdk.setBusy?.(editor.busy);
        }
        const load = async () => {
            try {
                await sdk.assets?.stylesheet('remote-session.css');
                if (disposed || sdk.signal.aborted)
                    return;
                if (!canRead)
                    throw new Error('Your account cannot read remote session settings.');
                await editor.refresh();
            }
            catch (error) {
                if (!disposed && !sdk.signal.aborted) {
                    notice.textContent = RemoteSessionUi.errorMessage(error);
                    notice.dataset.error = 'true';
                }
            }
        };
        void load();
        return dispose;
    }
    RemoteSessionUi.mount = mount;
    window.AmaterasuExtensions.v1.register('remote-session', {
        title: 'Remote sessions',
        mountSettings: mount,
    });
})(RemoteSessionUi || (RemoteSessionUi = {}));

})();
