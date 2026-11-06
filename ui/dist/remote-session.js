(function(){
"use strict";
var RemoteSessionUi;
(function (RemoteSessionUi) {
    function managementObject(value) {
        if (value === null || typeof value !== 'object' || Array.isArray(value)) {
            throw new Error('The monitor service returned an invalid document.');
        }
        return value;
    }
    RemoteSessionUi.managementObject = managementObject;
    function managementText(value, maximum, allowEmpty = false) {
        if (typeof value !== 'string' || (!allowEmpty && value.trim().length === 0)
            || new TextEncoder().encode(value).length > maximum || /[\u0000-\u001f\u007f]/u.test(value)) {
            throw new Error('The monitor service returned an invalid name or identity.');
        }
        return value;
    }
    RemoteSessionUi.managementText = managementText;
    function managementRevision(value) {
        if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
            throw new Error('The document revision cannot be represented safely in this editor.');
        }
        return value;
    }
    RemoteSessionUi.managementRevision = managementRevision;
    function managementInteger(value, minimum, maximum, message) {
        if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < minimum || value > maximum) {
            throw new Error(message);
        }
        return value;
    }
    RemoteSessionUi.managementInteger = managementInteger;
    function optionalBoolean(value) {
        if (value !== undefined && typeof value !== 'boolean')
            throw new Error('The monitor placement contains an invalid switch.');
        return value === true;
    }
    function parsePlacement(value) {
        const source = managementObject(value);
        if (source.anchor_kind !== 'physical' && source.anchor_kind !== 'client')
            throw new Error('Choose a display or client anchor.');
        if (!['left', 'right', 'above', 'below'].includes(String(source.edge)))
            throw new Error('Choose a monitor edge.');
        if (!['start', 'center', 'end'].includes(String(source.alignment)))
            throw new Error('Choose a monitor alignment.');
        const placement = {
            anchor_kind: source.anchor_kind,
            anchor_id: managementText(source.anchor_id, 4096, true),
            edge: source.edge,
            alignment: source.alignment,
            gap_px: managementInteger(source.gap_px, 0, 10_000, 'The monitor gap must be a whole number from 0 to 10000 pixels.'),
            primary: optionalBoolean(source.primary), preserve: optionalBoolean(source.preserve),
            automatic_fallback: optionalBoolean(source.automatic_fallback),
        };
        if (!placement.preserve && placement.anchor_id.length === 0)
            throw new Error('Choose an anchor for the relative position.');
        if (!placement.preserve)
            placement.automatic_fallback = false;
        return placement;
    }
    RemoteSessionUi.parsePlacement = parsePlacement;
    function emptyLayout() { return { version: 1, placements: Object.create(null) }; }
    RemoteSessionUi.emptyLayout = emptyLayout;
    function automaticPlacement() {
        return { anchor_kind: 'physical', anchor_id: '', edge: 'right', alignment: 'start',
            gap_px: 0, primary: false, preserve: true, automatic_fallback: true };
    }
    RemoteSessionUi.automaticPlacement = automaticPlacement;
    function parseLayout(value) {
        const source = managementObject(value);
        if (source.version !== 1)
            throw new Error('This monitor layout version is unsupported.');
        const placements = managementObject(source.placements);
        if (Object.keys(placements).length > 32)
            throw new Error('A monitor layout can contain at most 32 clients.');
        const result = emptyLayout();
        let primaryCount = 0;
        for (const id of Object.keys(placements).sort()) {
            managementText(id, 128);
            Object.defineProperty(result.placements, id, {
                enumerable: true, configurable: true, writable: true, value: parsePlacement(placements[id]),
            });
            primaryCount += Number(result.placements[id].primary);
        }
        if (primaryCount > 1)
            throw new Error('Choose only one primary monitor.');
        if (new TextEncoder().encode(JSON.stringify(result)).length > 12_000)
            throw new Error('The saved monitor layout is too large.');
        for (const id of Object.keys(result.placements)) {
            const visited = new Set();
            let current = id;
            while (Object.hasOwn(result.placements, current)) {
                const placement = result.placements[current];
                if (placement.preserve || placement.anchor_kind === 'physical')
                    break;
                if (visited.has(current))
                    throw new Error('Monitor anchors cannot form a loop.');
                visited.add(current);
                current = placement.anchor_id;
            }
        }
        return result;
    }
    RemoteSessionUi.parseLayout = parseLayout;
    function layoutEqual(left, right) {
        return JSON.stringify(parseLayout(left)) === JSON.stringify(parseLayout(right));
    }
    RemoteSessionUi.layoutEqual = layoutEqual;
    function parseUniqueList(value, parse) {
        if (!Array.isArray(value) || value.length > 64)
            throw new Error('The monitor inventory exceeds this editor’s limit.');
        const entries = value.map(parse);
        if (new Set(entries.map(entry => entry.id)).size !== entries.length)
            throw new Error('The monitor inventory contains duplicate identities.');
        return entries;
    }
    function parseLayoutReply(value) {
        const error = RemoteSessionUi.requestError(value);
        if (error)
            throw error;
        const source = managementObject(value);
        if (source.ok !== true)
            throw new Error('The layout service did not confirm success.');
        const clients = parseUniqueList(source.clients, value => {
            const client = managementObject(value);
            if (typeof client.ready !== 'boolean')
                throw new Error('The monitor readiness report is invalid.');
            return { id: managementText(client.id, 128), label: managementText(client.label, 160),
                ready: client.ready, state: managementText(client.state, 128),
                ...(client.error === undefined ? {} : { error: managementText(client.error, 1024) }) };
        });
        const anchors = parseUniqueList(source.physical_anchors, value => {
            const anchor = managementObject(value);
            return { id: managementText(anchor.id, 4096), label: managementText(anchor.label, 160) };
        });
        const status = managementObject(source.application_status);
        return { ok: true, revision: managementRevision(source.revision), layout: parseLayout(source.layout),
            clients, physical_anchors: anchors,
            application_status: { state: managementText(status.state, 128),
                ...(status.error === undefined ? {} : { error: managementText(status.error, 1024) }) } };
    }
    RemoteSessionUi.parseLayoutReply = parseLayoutReply;
    function validateLayoutChoices(layout, inventory) {
        const clients = new Set(inventory.clients.map(client => client.id));
        const anchors = new Set(inventory.physical_anchors.map(anchor => anchor.id));
        for (const [id, placement] of Object.entries(layout.placements)) {
            if (!clients.has(id))
                throw new Error('A saved position belongs to an unavailable client. Clear its saved rule before saving.');
            if (placement.preserve)
                continue;
            if (placement.anchor_kind === 'physical' ? !anchors.has(placement.anchor_id)
                : placement.anchor_id === id || !clients.has(placement.anchor_id)) {
                throw new Error('A monitor anchor is unavailable. Choose another anchor before saving.');
            }
        }
    }
    RemoteSessionUi.validateLayoutChoices = validateLayoutChoices;
    class LayoutEditor {
        api;
        changed;
        saved = null;
        draft = null;
        busy = false;
        error = false;
        message = '';
        confirmationRequired = false;
        disposed = false;
        generation = 0;
        inventoryGeneration = 0;
        inventoryPending = false;
        constructor(api, changed) {
            this.api = api;
            this.changed = changed;
        }
        dirty() {
            if (!this.saved || !this.draft)
                return false;
            try {
                return !layoutEqual(this.saved.layout, this.draft);
            }
            catch {
                return true;
            }
        }
        edit(clientId, placement) {
            if (this.disposed || this.busy || this.draft === null)
                return;
            const placements = Object.assign(Object.create(null), this.draft.placements);
            if (placement === null)
                delete placements[clientId];
            else {
                if (placement.primary) {
                    for (const [id, current] of Object.entries(placements))
                        placements[id] = { ...current, primary: false };
                }
                Object.defineProperty(placements, clientId, { enumerable: true, configurable: true, writable: true, value: { ...placement } });
            }
            this.draft = { version: 1, placements };
            this.message = '';
            this.error = false;
            this.changed();
        }
        validationError() {
            try {
                if (!this.draft || !this.saved)
                    return 'Read the saved monitor layout first.';
                validateLayoutChoices(parseLayout(this.draft), this.saved);
                return null;
            }
            catch (error) {
                return RemoteSessionUi.errorMessage(error);
            }
        }
        async refresh(discardDraft = false) {
            if (this.disposed || this.busy)
                return;
            const generation = ++this.generation;
            this.inventoryGeneration++;
            this.busy = true;
            this.error = false;
            this.message = 'Reading saved monitor positions…';
            this.changed();
            try {
                const current = parseLayoutReply(await this.api.call('layout'));
                if (!this.active(generation))
                    return;
                const placements = Object.assign(Object.create(null), current.layout.placements);
                if (!discardDraft && this.saved && this.draft) {
                    const ids = new Set([...Object.keys(this.saved.layout.placements), ...Object.keys(this.draft.placements)]);
                    for (const id of ids) {
                        const previous = this.saved.layout.placements[id], draft = this.draft.placements[id];
                        if (JSON.stringify(previous) === JSON.stringify(draft))
                            continue;
                        if (draft === undefined)
                            delete placements[id];
                        else
                            Object.defineProperty(placements, id, { enumerable: true, configurable: true, writable: true, value: { ...draft } });
                    }
                }
                this.saved = current;
                this.draft = { version: 1, placements };
                this.confirmationRequired = false;
                this.message = this.dirty() ? 'Saved positions refreshed. Your unsaved changes are kept.' : 'Saved monitor positions are up to date.';
            }
            catch (error) {
                if (this.active(generation)) {
                    this.message = RemoteSessionUi.errorMessage(error);
                    this.error = true;
                }
            }
            finally {
                if (this.active(generation)) {
                    this.busy = false;
                    this.changed();
                }
            }
        }
        async pollInventory() {
            if (this.disposed || this.busy || this.inventoryPending || !this.saved)
                return;
            const generation = this.inventoryGeneration;
            this.inventoryPending = true;
            try {
                const current = parseLayoutReply(await this.api.call('layout'));
                if (this.disposed || generation !== this.inventoryGeneration || !this.saved)
                    return;
                this.saved = { ...this.saved, clients: current.clients, physical_anchors: current.physical_anchors,
                    application_status: current.application_status };
                this.changed();
            }
            catch {
                if (!this.disposed && generation === this.inventoryGeneration && this.saved) {
                    this.saved = { ...this.saved, application_status: { state: 'unavailable', error: 'Current monitor status could not be refreshed.' } };
                    this.changed();
                }
            }
            finally {
                this.inventoryPending = false;
            }
        }
        async save() {
            if (this.disposed || this.busy || !this.saved || !this.draft || !this.dirty() || this.confirmationRequired)
                return;
            const invalid = this.validationError();
            if (invalid) {
                this.message = invalid;
                this.error = true;
                this.changed();
                return;
            }
            const requested = parseLayout(this.draft), revision = this.saved.revision;
            if (!Number.isSafeInteger(revision + 1)) {
                this.message = 'The next layout revision cannot be represented safely.';
                this.error = true;
                this.changed();
                return;
            }
            const generation = ++this.generation;
            this.inventoryGeneration++;
            this.busy = true;
            this.error = false;
            this.message = 'Saving monitor positions…';
            this.changed();
            let rejected = false;
            try {
                const result = await this.api.call('layout_update', { layout: requested, expected_revision: revision });
                if (!this.active(generation))
                    return;
                const rejection = RemoteSessionUi.requestError(result);
                rejected = rejection !== null;
                if (rejection)
                    throw rejection;
                const updated = parseLayoutReply(result);
                if (updated.revision !== revision + 1 || !layoutEqual(updated.layout, requested))
                    throw new Error('The update did not confirm the requested layout.');
                const readback = parseLayoutReply(await this.api.call('layout'));
                if (!this.active(generation))
                    return;
                if (readback.revision !== updated.revision || !layoutEqual(readback.layout, requested))
                    throw new Error('The saved layout changed before readback.');
                this.saved = readback;
                this.draft = parseLayout(readback.layout);
                this.confirmationRequired = false;
                this.message = 'Monitor positions saved and confirmed.';
            }
            catch (error) {
                if (!this.active(generation))
                    return;
                this.error = true;
                this.message = RemoteSessionUi.errorMessage(error);
                this.confirmationRequired = !rejected;
                if (this.confirmationRequired)
                    this.message += ' Refresh saved positions before saving again; your draft is kept.';
            }
            finally {
                if (this.active(generation)) {
                    this.busy = false;
                    this.changed();
                }
            }
        }
        dispose() { this.disposed = true; this.generation++; this.inventoryGeneration++; }
        active(generation) { return !this.disposed && generation === this.generation; }
    }
    RemoteSessionUi.LayoutEditor = LayoutEditor;
})(RemoteSessionUi || (RemoteSessionUi = {}));
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
        result.remote_monitor_layout = source.remote_monitor_layout === undefined ? RemoteSessionUi.emptyLayout() : RemoteSessionUi.parseLayout(source.remote_monitor_layout);
        return result;
    }
    RemoteSessionUi.parseSettings = parseSettings;
    function settingsEqual(left, right) {
        return RemoteSessionUi.booleanSettings.every(name => left[name] === right[name])
            && left.max_clients === right.max_clients
            && RemoteSessionUi.layoutEqual(left.remote_monitor_layout, right.remote_monitor_layout);
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
    RemoteSessionUi.profileScales = [-1, 0, 100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];
    function newProfile() {
        return { id: '', uuid: '', name: '', width: 1920, height: 1080, refresh_millihz: 60_000,
            hdr_enabled: false, scale: 100, target: { kind: 'owned' } };
    }
    RemoteSessionUi.newProfile = newProfile;
    function parseProfile(value, creating = false) {
        const source = RemoteSessionUi.managementObject(value);
        const id = RemoteSessionUi.managementText(source.id, 24, creating);
        if (id !== '' && !/^app-[0-9a-f]{16}$/u.test(id))
            throw new Error('The profile identity is invalid.');
        const uuid = RemoteSessionUi.managementText(source.uuid, 36, creating);
        if (uuid !== '' && (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/iu.test(uuid)
            || uuid === '00000000-0000-0000-0000-000000000000'))
            throw new Error('The profile identity is invalid.');
        if (creating && (id !== '' || uuid !== ''))
            throw new Error('New profile identities must be assigned by the catalog.');
        if (typeof source.hdr_enabled !== 'boolean')
            throw new Error('Choose whether the profile uses HDR.');
        const scale = RemoteSessionUi.managementInteger(source.scale, -1, 500, 'Choose a supported display scale.');
        if (!RemoteSessionUi.profileScales.includes(scale))
            throw new Error('Choose a supported display scale.');
        const targetSource = source.target === undefined ? { kind: 'owned' } : RemoteSessionUi.managementObject(source.target);
        let target;
        if (targetSource.kind === 'owned')
            target = { kind: 'owned' };
        else if (targetSource.kind === 'existing')
            target = { kind: 'existing', selector: RemoteSessionUi.managementText(targetSource.selector, 256) };
        else
            throw new Error('Choose a supported display target.');
        return { id, uuid, name: RemoteSessionUi.managementText(source.name, 160),
            width: RemoteSessionUi.managementInteger(source.width, 320, 7680, 'Width must be a whole number from 320 to 7680 pixels.'),
            height: RemoteSessionUi.managementInteger(source.height, 200, 4320, 'Height must be a whole number from 200 to 4320 pixels.'),
            refresh_millihz: RemoteSessionUi.managementInteger(source.refresh_millihz, 1000, 1_000_000, 'Refresh rate must be from 1 to 1000 Hz, with up to three decimal places.'),
            hdr_enabled: source.hdr_enabled, scale, target };
    }
    RemoteSessionUi.parseProfile = parseProfile;
    function parseProfilesReply(value) {
        const error = RemoteSessionUi.requestError(value);
        if (error)
            throw error;
        const source = RemoteSessionUi.managementObject(value);
        if (source.ok !== true || !Array.isArray(source.profiles) || source.profiles.length > 16)
            throw new Error('The profile list is unavailable or exceeds this editor’s limit.');
        const profiles = source.profiles.map(profile => parseProfile(profile));
        if (new Set(profiles.map(profile => profile.id)).size !== profiles.length
            || new Set(profiles.map(profile => profile.uuid.toLowerCase())).size !== profiles.length)
            throw new Error('The profile list contains duplicate identities.');
        return { ok: true, revision: RemoteSessionUi.managementRevision(source.revision), profiles };
    }
    RemoteSessionUi.parseProfilesReply = parseProfilesReply;
    function parseProfileTargets(value) {
        const error = RemoteSessionUi.requestError(value);
        if (error)
            throw error;
        const source = RemoteSessionUi.managementObject(value);
        if (source.ok !== true || !Array.isArray(source.targets) || source.targets.length > 32)
            throw new Error('The display target list is unavailable.');
        const targets = source.targets.map(value => {
            const target = RemoteSessionUi.managementObject(value);
            if (typeof target.ready !== 'boolean')
                throw new Error('The target readiness report is invalid.');
            return { selector: RemoteSessionUi.managementText(target.selector, 256), label: RemoteSessionUi.managementText(target.label, 160), ready: target.ready,
                ...(target.error === undefined ? {} : { error: RemoteSessionUi.managementText(target.error, 1024) }) };
        });
        if (new Set(targets.map(target => target.selector)).size !== targets.length)
            throw new Error('The target list contains duplicate identities.');
        return targets;
    }
    RemoteSessionUi.parseProfileTargets = parseProfileTargets;
    function profileEqual(left, right) {
        return JSON.stringify(left) === JSON.stringify(right);
    }
    RemoteSessionUi.profileEqual = profileEqual;
    class ProfileEditor {
        api;
        changed;
        saved = null;
        draft = null;
        targets = [];
        targetError = '';
        busy = false;
        error = false;
        message = '';
        confirmationRequired = false;
        disposed = false;
        generation = 0;
        original = null;
        creationBaseline = null;
        constructor(api, changed) {
            this.api = api;
            this.changed = changed;
        }
        dirty() {
            return this.draft !== null && (this.original === null || !profileEqual(this.draft, this.original));
        }
        select(id) {
            if (this.disposed || this.busy || this.dirty() || !this.saved)
                return;
            const profile = this.saved.profiles.find(profile => profile.id === id);
            this.original = profile ? { ...profile, target: { ...profile.target } } : null;
            this.draft = profile ? { ...profile, target: { ...profile.target } } : newProfile();
            this.error = false;
            this.message = '';
            this.changed();
        }
        edit(patch) {
            if (this.disposed || this.busy || !this.draft)
                return;
            this.draft = { ...this.draft, ...patch };
            this.error = false;
            this.message = '';
            this.changed();
        }
        validationError() {
            try {
                if (!this.draft)
                    return 'Choose a profile to edit.';
                const profile = parseProfile(this.draft, this.draft.id === '');
                if (profile.id !== '' && !this.saved?.profiles.some(current => current.id === profile.id)) {
                    return 'This profile was deleted in another editor. Discard the draft before selecting another profile.';
                }
                const target = profile.target;
                if (target.kind === 'existing' && !this.targets.some(current => current.selector === target.selector)
                    && JSON.stringify(target) !== JSON.stringify(this.original?.target)) {
                    return 'The selected display is unavailable. Refresh display targets or choose a client-owned monitor.';
                }
                return null;
            }
            catch (error) {
                return RemoteSessionUi.errorMessage(error);
            }
        }
        async refresh(discardDraft = false) {
            if (this.disposed || this.busy)
                return;
            const generation = ++this.generation;
            this.busy = true;
            this.error = false;
            this.message = 'Reading monitor profiles…';
            this.changed();
            try {
                const current = parseProfilesReply(await this.api.call('profiles'));
                if (!this.active(generation))
                    return;
                let draft = this.draft;
                let keep = !discardDraft && this.dirty();
                let ambiguousCreate = false;
                if (this.confirmationRequired && draft?.id === '' && this.creationBaseline !== null) {
                    const requested = draft;
                    const matches = current.profiles.filter(profile => !this.creationBaseline.has(profile.id)
                        && profileEqual({ ...profile, id: '', uuid: '' }, requested));
                    if (matches.length === 1) {
                        draft = matches[0];
                        keep = false;
                    }
                    else if (matches.length > 1)
                        ambiguousCreate = true;
                }
                this.saved = current;
                if (keep && draft) {
                    const previous = this.original;
                    this.original = current.profiles.find(profile => profile.id === draft.id) ?? null;
                    if (previous && this.original) {
                        const merged = { ...this.original };
                        for (const name of ['name', 'width', 'height', 'refresh_millihz', 'hdr_enabled', 'scale', 'target']) {
                            if (JSON.stringify(previous[name]) !== JSON.stringify(draft[name])) {
                                Object.assign(merged, { [name]: draft[name] });
                            }
                        }
                        this.draft = merged;
                    }
                    else
                        this.draft = draft;
                }
                else {
                    const profile = current.profiles.find(profile => profile.id === draft?.id) ?? current.profiles[0] ?? null;
                    this.original = profile;
                    this.draft = profile ? { ...profile, target: { ...profile.target } } : null;
                }
                this.confirmationRequired = ambiguousCreate;
                if (!ambiguousCreate)
                    this.creationBaseline = null;
                this.message = ambiguousCreate ? 'More than one matching saved profile exists. Discard this draft and select the intended profile before making another change.'
                    : this.dirty() ? 'Profiles refreshed. Your unsaved changes are kept.' : 'Monitor profiles are up to date.';
                this.error = ambiguousCreate;
            }
            catch (error) {
                if (this.active(generation)) {
                    this.error = true;
                    this.message = RemoteSessionUi.errorMessage(error);
                }
            }
            finally {
                if (this.active(generation)) {
                    this.busy = false;
                    this.changed();
                }
            }
        }
        async refreshTargets() {
            if (this.disposed || this.busy)
                return;
            const generation = this.generation;
            try {
                const targets = parseProfileTargets(await this.api.call('profile_targets'));
                if (!this.active(generation))
                    return;
                this.targets = targets;
                this.targetError = '';
                this.changed();
            }
            catch (error) {
                if (this.active(generation)) {
                    this.targetError = RemoteSessionUi.errorMessage(error);
                    this.changed();
                }
            }
        }
        async save() {
            if (this.disposed || this.busy || !this.saved || !this.draft || !this.dirty() || this.confirmationRequired)
                return;
            const invalid = this.validationError();
            if (invalid) {
                this.error = true;
                this.message = invalid;
                this.changed();
                return;
            }
            const requested = parseProfile(this.draft, this.draft.id === ''), revision = this.saved.revision;
            const generation = ++this.generation;
            this.creationBaseline = requested.id === '' ? new Set(this.saved.profiles.map(profile => profile.id)) : null;
            this.busy = true;
            this.error = false;
            this.message = 'Saving monitor profile…';
            this.changed();
            let rejected = false;
            try {
                const response = await this.api.call('profile_upsert', { profile: requested, expected_revision: revision });
                if (!this.active(generation))
                    return;
                const rejection = RemoteSessionUi.requestError(response);
                rejected = rejection !== null;
                if (rejection) {
                    this.creationBaseline = null;
                    throw rejection;
                }
                const updated = RemoteSessionUi.managementObject(response);
                if (updated.ok !== true)
                    throw new Error('The profile service did not confirm the update.');
                const confirmed = parseProfile(updated.profile), updatedRevision = RemoteSessionUi.managementRevision(updated.revision);
                if (updatedRevision <= revision || (requested.id !== '' && (confirmed.id !== requested.id || confirmed.uuid !== requested.uuid))
                    || !profileEqual({ ...confirmed, id: requested.id, uuid: requested.uuid }, requested))
                    throw new Error('The profile update did not confirm the requested values.');
                const readback = parseProfilesReply(await this.api.call('profiles'));
                if (!this.active(generation))
                    return;
                const profile = readback.profiles.find(profile => profile.id === confirmed.id);
                if (readback.revision !== updatedRevision || !profile || !profileEqual(profile, confirmed))
                    throw new Error('The saved profile changed before readback.');
                this.saved = readback;
                this.original = profile;
                this.draft = { ...profile, target: { ...profile.target } };
                this.confirmationRequired = false;
                this.creationBaseline = null;
                this.message = 'Monitor profile saved and confirmed.';
            }
            catch (error) {
                if (!this.active(generation))
                    return;
                this.error = true;
                this.message = RemoteSessionUi.errorMessage(error);
                this.confirmationRequired = !rejected;
                if (this.confirmationRequired)
                    this.message += ' Refresh profiles before saving again; your draft is kept.';
            }
            finally {
                if (this.active(generation)) {
                    this.busy = false;
                    this.changed();
                }
            }
        }
        async remove() {
            if (this.disposed || this.busy || this.dirty() || !this.saved || !this.draft || this.draft.id === '' || this.confirmationRequired)
                return;
            const id = this.draft.id, name = this.draft.name, revision = this.saved.revision, generation = ++this.generation;
            this.busy = true;
            this.error = false;
            this.message = 'Deleting monitor profile…';
            this.changed();
            let rejected = false;
            try {
                const response = await this.api.call('profile_delete', { id, expected_revision: revision });
                if (!this.active(generation))
                    return;
                const rejection = RemoteSessionUi.requestError(response);
                rejected = rejection !== null;
                if (rejection)
                    throw rejection;
                const updated = RemoteSessionUi.managementObject(response), updatedRevision = RemoteSessionUi.managementRevision(updated.revision);
                if (updated.ok !== true || updatedRevision <= revision)
                    throw new Error('The profile service did not confirm deletion.');
                const readback = parseProfilesReply(await this.api.call('profiles'));
                if (!this.active(generation))
                    return;
                if (readback.revision !== updatedRevision || readback.profiles.some(profile => profile.id === id))
                    throw new Error('The deleted profile is still present in readback.');
                this.saved = readback;
                this.original = readback.profiles[0] ?? null;
                this.draft = this.original ? { ...this.original, target: { ...this.original.target } } : null;
                this.confirmationRequired = false;
                this.message = `Monitor profile “${name}” deleted and confirmed. Retained monitors keep their accepted settings.`;
            }
            catch (error) {
                if (!this.active(generation))
                    return;
                this.error = true;
                this.message = RemoteSessionUi.errorMessage(error);
                this.confirmationRequired = !rejected;
                if (this.confirmationRequired)
                    this.message += ' Refresh profiles to check the deletion before trying again.';
            }
            finally {
                if (this.active(generation)) {
                    this.busy = false;
                    this.changed();
                }
            }
        }
        dispose() { this.disposed = true; this.generation++; }
        active(generation) { return !this.disposed && generation === this.generation; }
    }
    RemoteSessionUi.ProfileEditor = ProfileEditor;
})(RemoteSessionUi || (RemoteSessionUi = {}));
var RemoteSessionUi;
(function (RemoteSessionUi) {
    class EditorGuards {
        changed;
        editors = new Map();
        constructor(changed) {
            this.changed = changed;
        }
        create() {
            const key = {};
            let disposed = false;
            this.editors.set(key, { dirty: false, busy: false });
            return {
                update: (dirty, busy) => {
                    if (disposed)
                        return;
                    this.editors.set(key, { dirty, busy });
                    this.publish();
                },
                dispose: () => {
                    if (disposed)
                        return;
                    disposed = true;
                    this.editors.delete(key);
                    this.publish();
                },
            };
        }
        publish() {
            this.changed([...this.editors.values()].some(editor => editor.dirty), [...this.editors.values()].some(editor => editor.busy));
        }
    }
    RemoteSessionUi.EditorGuards = EditorGuards;
})(RemoteSessionUi || (RemoteSessionUi = {}));
var RemoteSessionUi;
(function (RemoteSessionUi) {
    function monitorText(tag, content, className = '') {
        const element = document.createElement(tag);
        element.textContent = content;
        element.className = className;
        return element;
    }
    function placementMode(placement) {
        return !placement.preserve ? 'relative' : placement.automatic_fallback ? 'automatic' : 'keep';
    }
    function mountLayout(container, sdk, guard) {
        const ui = sdk.ui;
        if (!ui || !sdk.canCall?.('layout')) {
            guard.dispose();
            return () => { };
        }
        const card = ui.card({ label: 'Monitor positions' });
        card.content.classList.add('remote-session-fields');
        card.element.classList.add('remote-session-layout');
        const description = monitorText('p', 'Choose where each client’s monitor appears beside host displays or other clients. Active monitors may need to reconnect before a saved position takes effect.');
        const notice = monitorText('p', 'Reading saved positions…', 'remote-session-notice');
        notice.setAttribute('role', 'status');
        notice.setAttribute('aria-live', 'polite');
        const readiness = monitorText('p', '', 'remote-session-readiness');
        const application = monitorText('p', '', 'remote-session-readiness');
        const preview = monitorText('p', '', 'remote-session-layout-preview');
        card.append(description);
        card.append(notice);
        const fields = [];
        const actions = [];
        let selected = null;
        let clientChoicesSignature = '', anchorChoicesSignature = '';
        let disposed = false;
        let timer = null;
        const canWrite = sdk.canCall('layout_update');
        const editor = new RemoteSessionUi.LayoutEditor(sdk, render);
        const client = ui.select({ label: 'Client monitor', choices: [{ value: '__none__', label: 'No clients available' }],
            value: '__none__', disabled: true, onChange: value => { selected = value === '__none__' ? null : value.slice(7); render(); } });
        const mode = ui.select({ label: 'Position', choices: [
                { value: 'automatic', label: 'Automatic — append to the right' },
                { value: 'relative', label: 'Beside a display or client' },
                { value: 'keep', label: 'Keep its current position' },
            ], value: 'automatic', disabled: true, onChange: value => {
                if (!selected || !editor.draft)
                    return;
                const current = editor.draft.placements[selected] ?? RemoteSessionUi.automaticPlacement();
                if (value === 'keep')
                    editor.edit(selected, { ...RemoteSessionUi.automaticPlacement(), automatic_fallback: false });
                else if (value === 'automatic')
                    editor.edit(selected, { ...RemoteSessionUi.automaticPlacement(), primary: current.primary });
                else {
                    const first = anchorChoices()[0];
                    const raw = first?.value ?? '';
                    editor.edit(selected, { ...current, preserve: false, automatic_fallback: false,
                        anchor_kind: raw.startsWith('client:') ? 'client' : 'physical',
                        anchor_id: raw.startsWith('client:') ? raw.slice(7) : raw.startsWith('physical:') ? raw.slice(9) : '' });
                }
            } });
        const anchor = ui.select({ label: 'Anchor display or client', choices: [{ value: '__none__', label: 'No anchor available' }],
            value: '__none__', disabled: true, onChange: value => {
                if (value !== '__none__')
                    edit({ anchor_kind: value.startsWith('client:') ? 'client' : 'physical',
                        anchor_id: value.startsWith('client:') ? value.slice(7) : value.slice(9) });
            } });
        const edge = ui.select({ label: 'Place monitor', choices: [
                { value: 'left', label: 'To the left of the anchor' }, { value: 'right', label: 'To the right of the anchor' },
                { value: 'above', label: 'Above the anchor' }, { value: 'below', label: 'Below the anchor' },
            ], value: 'right', disabled: true, onChange: value => edit({ edge: value }) });
        const alignment = ui.select({ label: 'Align edges', choices: [
                { value: 'start', label: 'Start — top or left' }, { value: 'center', label: 'Center' }, { value: 'end', label: 'End — bottom or right' },
            ], value: 'start', disabled: true, onChange: value => edit({ alignment: value }) });
        const gap = ui.numberField({ label: 'Gap between monitors (pixels)', value: 0, minimum: 0, maximum: 10_000, step: 1,
            disabled: true, onChange: value => edit({ gap_px: value }) });
        const primary = ui.toggle({ label: 'Use this monitor as primary', value: false, disabled: true,
            help: 'Only one saved client monitor can be primary. A current-position rule leaves the existing primary unchanged.',
            onChange: value => edit({ primary: value }) });
        fields.push(client, mode, anchor, edge, alignment, gap, primary);
        for (const field of fields)
            card.append(field.element);
        card.append(readiness);
        card.append(preview);
        card.append(application);
        const buttons = document.createElement('div');
        buttons.className = 'remote-session-actions';
        const save = ui.button({ label: 'Save monitor positions', variant: 'primary', disabled: true, onClick: () => editor.save() });
        const refresh = ui.button({ label: 'Refresh saved positions', onClick: () => editor.refresh() });
        const discard = ui.button({ label: 'Discard position changes', disabled: true, onClick: () => editor.refresh(true) });
        const clear = ui.button({ label: 'Clear this client’s saved rule', disabled: true,
            onClick: () => { if (selected)
                editor.edit(selected, null); } });
        actions.push(save, refresh, discard, clear);
        buttons.append(...actions.map(action => action.element));
        card.append(buttons);
        container.append(card.element);
        function edit(patch) {
            if (!selected || !editor.draft)
                return;
            editor.edit(selected, { ...(editor.draft.placements[selected] ?? RemoteSessionUi.automaticPlacement()), ...patch });
        }
        function anchorChoices() {
            if (!editor.saved)
                return [];
            return [
                ...editor.saved.physical_anchors.map(anchor => ({ value: `physical:${anchor.id}`, label: `Host display — ${anchor.label}` })),
                ...editor.saved.clients.filter(client => client.id !== selected).map(client => ({ value: `client:${client.id}`, label: `Remote client — ${client.label}` })),
            ];
        }
        function render() {
            if (disposed || sdk.signal.aborted)
                return;
            const saved = editor.saved, draft = editor.draft;
            const choices = saved?.clients.map(client => ({ value: `client:${client.id}`, label: client.label })) ?? [];
            if (draft) {
                for (const id of Object.keys(draft.placements)) {
                    if (!saved?.clients.some(client => client.id === id))
                        choices.push({ value: `client:${id}`, label: 'Unavailable saved client' });
                }
            }
            if (!choices.some(choice => choice.value === `client:${selected}`))
                selected = choices[0]?.value.slice(7) ?? null;
            const clientChoices = choices.length ? choices : [{ value: '__none__', label: 'No clients available' }];
            const clientValue = selected ? `client:${selected}` : '__none__';
            const clientSignature = JSON.stringify([clientChoices, clientValue]);
            if (clientSignature !== clientChoicesSignature) {
                client.setChoices(clientChoices, clientValue);
                clientChoicesSignature = clientSignature;
            }
            client.setState({ disabled: editor.busy || choices.length === 0 });
            const placement = selected && draft ? draft.placements[selected] ?? RemoteSessionUi.automaticPlacement() : RemoteSessionUi.automaticPlacement();
            const currentMode = placementMode(placement);
            mode.setValue(currentMode);
            const anchors = anchorChoices();
            const anchorValue = `${placement.anchor_kind}:${placement.anchor_id}`;
            if (placement.anchor_id && !anchors.some(choice => choice.value === anchorValue))
                anchors.push({ value: anchorValue, label: 'Unavailable saved anchor — choose another' });
            const chosenAnchor = placement.anchor_id ? anchorValue : anchors[0]?.value ?? '__none__';
            const availableAnchors = anchors.length ? anchors : [{ value: '__none__', label: 'No anchor available' }];
            const anchorSignature = JSON.stringify([availableAnchors, chosenAnchor]);
            if (anchorSignature !== anchorChoicesSignature) {
                anchor.setChoices(availableAnchors, chosenAnchor);
                anchorChoicesSignature = anchorSignature;
            }
            edge.setValue(placement.edge);
            alignment.setValue(placement.alignment);
            if (!Object.is(gap.value(), placement.gap_px))
                gap.setValue(placement.gap_px);
            primary.setValue(placement.primary);
            const disabled = editor.busy || !canWrite || !selected || !draft;
            mode.setState({ disabled });
            for (const field of [anchor, edge, alignment, gap])
                field.setState({ disabled: disabled || currentMode !== 'relative' });
            primary.setState({ disabled: disabled || currentMode === 'keep' });
            const invalid = editor.validationError();
            gap.setState({ error: !Number.isSafeInteger(placement.gap_px) || placement.gap_px < 0 || placement.gap_px > 10_000
                    ? 'Enter a whole number from 0 to 10000.' : null });
            save.setState({ disabled: disabled || !editor.dirty() || invalid !== null || editor.confirmationRequired, busy: editor.busy });
            refresh.setState({ disabled: editor.busy });
            discard.setState({ disabled: editor.busy || !editor.dirty() });
            clear.setState({ disabled: disabled || !selected || !draft || !Object.hasOwn(draft.placements, selected) });
            notice.textContent = editor.message || (editor.dirty() && invalid ? invalid : '');
            notice.dataset.error = String(editor.error || editor.dirty() && invalid !== null);
            const owner = saved?.clients.find(client => client.id === selected);
            readiness.textContent = owner ? `${owner.label}: ${owner.ready ? 'ready' : owner.state.replaceAll('_', ' ')}${owner.error ? ` — ${owner.error}` : ''}.`
                : selected ? 'This client is unavailable. Its saved rule can be cleared.' : 'Paired clients appear here after their monitor inventory is available.';
            if (currentMode === 'relative') {
                const label = anchors.find(choice => choice.value === anchorValue)?.label ?? 'an unavailable anchor';
                const relation = { left: 'left of', right: 'right of', above: 'above', below: 'below' }[placement.edge];
                preview.textContent = `Position: ${relation} ${label}; ${placement.alignment} alignment; ${placement.gap_px} px gap${placement.primary ? '; primary monitor' : ''}.`;
            }
            else
                preview.textContent = currentMode === 'keep' ? 'Position: leave the current desktop arrangement unchanged.'
                    : `Position: append to the right of the desktop${placement.primary ? '; primary monitor' : ''}.`;
            const status = saved?.application_status;
            application.textContent = status ? `Placement status: ${status.state.replaceAll('_', ' ')}${status.error ? ` — ${status.error}` : ''}.` : '';
            application.dataset.error = String(Boolean(status?.error));
            guard.update(editor.dirty(), editor.busy);
        }
        const poll = async () => {
            if (disposed || sdk.signal.aborted)
                return;
            if (document.visibilityState !== 'hidden')
                await editor.pollInventory();
            if (!disposed && !sdk.signal.aborted)
                timer = setTimeout(() => { void poll(); }, 2000);
        };
        const dispose = () => {
            if (disposed)
                return;
            disposed = true;
            editor.dispose();
            if (timer !== null)
                clearTimeout(timer);
            for (const field of fields)
                field.dispose();
            for (const action of actions)
                action.dispose();
            card.dispose();
            guard.dispose();
            sdk.signal.removeEventListener('abort', dispose);
        };
        sdk.signal.addEventListener('abort', dispose, { once: true });
        void editor.refresh().then(() => { if (!disposed && !sdk.signal.aborted)
            timer = setTimeout(() => { void poll(); }, 2000); });
        return dispose;
    }
    RemoteSessionUi.mountLayout = mountLayout;
})(RemoteSessionUi || (RemoteSessionUi = {}));
var RemoteSessionUi;
(function (RemoteSessionUi) {
    function mountProfiles(container, sdk, guard) {
        const ui = sdk.ui;
        if (!ui || !sdk.canCall?.('profiles')) {
            guard.dispose();
            return () => { };
        }
        const card = ui.card({ label: 'Remote Monitor profiles' });
        card.content.classList.add('remote-session-fields');
        card.element.classList.add('remote-session-profiles');
        const paragraph = (content, className = '') => {
            const element = document.createElement('p');
            element.textContent = content;
            element.className = className;
            return element;
        };
        const description = paragraph('Create named monitor tiles for paired clients. Changing or deleting a profile affects future connections; a retained monitor keeps the settings it accepted until it disconnects.');
        const notice = paragraph('Reading monitor profiles…', 'remote-session-notice');
        notice.setAttribute('role', 'status');
        notice.setAttribute('aria-live', 'polite');
        const targetStatus = paragraph('', 'remote-session-readiness');
        const draftHint = paragraph('', 'remote-session-readiness');
        card.append(description);
        card.append(notice);
        const controls = [];
        const actions = [];
        let disposed = false;
        let dialog = null;
        let dialogActions = [];
        let profileChoicesSignature = '', targetChoicesSignature = '';
        const canWrite = sdk.canCall('profile_upsert'), canDelete = sdk.canCall('profile_delete');
        const canReadTargets = sdk.canCall('profile_targets');
        const editor = new RemoteSessionUi.ProfileEditor(sdk, render);
        const selector = ui.select({ label: 'Saved monitor profile', choices: [{ value: '__none__', label: 'No saved profiles' }],
            value: '__none__', disabled: true, onChange: value => editor.select(value === '__new__' ? '' : value.slice(8)) });
        const name = ui.textField({ label: 'Profile name', value: '', maxLength: 160, required: true, disabled: true,
            placeholder: 'For example: Drawing tablet', onChange: value => editor.edit({ name: value }) });
        const target = ui.select({ label: 'Display target', choices: [{ value: 'owned', label: 'Create a monitor for this client' }],
            value: 'owned', disabled: true, help: 'Existing displays are selected from the host’s verified inventory. Their identity is kept with the profile.',
            onChange: value => {
                if (value === 'owned')
                    editor.edit({ target: { kind: 'owned' } });
                else {
                    const selected = editor.targets.find(target => target.selector === value.slice(9));
                    if (selected?.ready)
                        editor.edit({ target: { kind: 'existing', selector: selected.selector } });
                    else
                        render();
                }
            } });
        const width = ui.numberField({ label: 'Monitor width (pixels)', value: 1920, minimum: 320, maximum: 7680, step: 1,
            required: true, disabled: true, onChange: value => editor.edit({ width: value }) });
        const height = ui.numberField({ label: 'Monitor height (pixels)', value: 1080, minimum: 200, maximum: 4320, step: 1,
            required: true, disabled: true, onChange: value => editor.edit({ height: value }) });
        const refreshRate = ui.numberField({ label: 'Monitor refresh rate (Hz)', value: 60, minimum: 1, maximum: 1000, step: .001,
            required: true, disabled: true, help: 'For example: 60, 59.94, or 120.',
            onChange: value => {
                const millihz = value * 1000;
                editor.edit({ refresh_millihz: Math.abs(millihz - Math.round(millihz)) < .000001 ? Math.round(millihz) : millihz });
            } });
        const hdr = ui.toggle({ label: 'Use HDR for this monitor', value: false, disabled: true,
            help: 'The selected display and capture provider must support the requested HDR state.', onChange: value => editor.edit({ hdr_enabled: value }) });
        const scale = ui.select({ label: 'Display scale', choices: RemoteSessionUi.profileScales.map(value => ({
                value: String(value), label: value <= 0 ? value === -1 ? 'Automatic (provider default)' : 'Automatic' : `${value}%`,
            })), value: '100', disabled: true, onChange: value => editor.edit({ scale: Number(value) }) });
        controls.push(selector, name, target, width, height, refreshRate, hdr, scale);
        for (const control of controls)
            card.append(control.element);
        card.append(targetStatus);
        card.append(draftHint);
        const buttons = document.createElement('div');
        buttons.className = 'remote-session-actions';
        const create = ui.button({ label: 'New monitor profile', disabled: true, onClick: () => editor.select('') });
        const save = ui.button({ label: 'Save monitor profile', variant: 'primary', disabled: true, onClick: () => editor.save() });
        const refresh = ui.button({ label: 'Refresh saved profiles', onClick: async () => {
                await editor.refresh();
                if (canReadTargets)
                    await editor.refreshTargets();
            } });
        const discard = ui.button({ label: 'Discard profile changes', disabled: true, onClick: () => editor.refresh(true) });
        const remove = ui.button({ label: 'Delete monitor profile', variant: 'danger', disabled: true, onClick: confirmDelete });
        actions.push(create, save, refresh, discard, remove);
        buttons.append(...actions.map(action => action.element));
        card.append(buttons);
        container.append(card.element);
        function closeDialog() {
            const current = dialog;
            dialog = null;
            current?.dispose();
            for (const action of dialogActions)
                action.dispose();
            dialogActions = [];
        }
        function confirmDelete() {
            if (!editor.draft || editor.busy || editor.dirty() || !canDelete || dialog)
                return;
            const message = paragraph(`Delete “${editor.draft.name}”? Clients will no longer see this profile tile. A monitor already using it keeps its accepted settings until disconnected.`);
            const confirm = ui.button({ label: 'Delete this profile', variant: 'danger', onClick: async () => {
                    await editor.remove();
                    closeDialog();
                } });
            dialogActions = [confirm];
            dialog = ui.dialog({ title: 'Delete monitor profile', content: [message, confirm.element],
                cancelLabel: 'Keep profile', onClose: closeDialog });
            card.append(dialog.element);
            dialog.open();
        }
        function render() {
            if (disposed || sdk.signal.aborted)
                return;
            const saved = editor.saved, draft = editor.draft;
            const choices = saved?.profiles.map(profile => {
                const duplicates = saved.profiles.filter(current => current.name === profile.name);
                const mode = `${profile.width} × ${profile.height}, ${profile.refresh_millihz / 1000} Hz${profile.hdr_enabled ? ', HDR' : ''}`;
                const sameMode = duplicates.filter(current => current.width === profile.width && current.height === profile.height
                    && current.refresh_millihz === profile.refresh_millihz && current.hdr_enabled === profile.hdr_enabled);
                const label = duplicates.length === 1 ? profile.name
                    : `${profile.name} — ${mode}${sameMode.length > 1 ? ` (${profile.id.slice(-6)})` : ''}`;
                return { value: `profile:${profile.id}`, label };
            }) ?? [];
            if (draft?.id === '')
                choices.unshift({ value: '__new__', label: 'New unsaved profile' });
            if (draft?.id && !saved?.profiles.some(profile => profile.id === draft.id))
                choices.push({ value: `profile:${draft.id}`, label: `${draft.name} — removed in another editor` });
            const value = draft ? draft.id === '' ? '__new__' : `profile:${draft.id}` : '__none__';
            if (choices.length === 0)
                choices.push({ value: '__none__', label: 'No saved profiles' });
            const choicesSignature = JSON.stringify([choices, value]);
            if (choicesSignature !== profileChoicesSignature) {
                selector.setChoices(choices, value);
                profileChoicesSignature = choicesSignature;
            }
            selector.setState({ disabled: editor.busy || editor.dirty() || !saved || saved.profiles.length === 0 });
            const disabled = editor.busy || !canWrite || !draft;
            for (const control of [name, target, width, height, refreshRate, hdr, scale])
                control.setState({ disabled });
            if (draft) {
                if (name.value() !== draft.name)
                    name.setValue(draft.name);
                if (!Object.is(width.value(), draft.width))
                    width.setValue(draft.width);
                if (!Object.is(height.value(), draft.height))
                    height.setValue(draft.height);
                if (!Object.is(refreshRate.value(), draft.refresh_millihz / 1000))
                    refreshRate.setValue(draft.refresh_millihz / 1000);
                hdr.setValue(draft.hdr_enabled);
                if (scale.value() !== String(draft.scale))
                    scale.setValue(String(draft.scale));
                const targets = [{ value: 'owned', label: 'Create a monitor for this client' },
                    ...editor.targets.filter(target => target.ready).map(target => ({ value: `existing:${target.selector}`, label: `Existing display — ${target.label}` }))];
                const targetValue = draft.target.kind === 'owned' ? 'owned' : `existing:${draft.target.selector}`;
                if (!targets.some(target => target.value === targetValue))
                    targets.push({ value: targetValue, label: 'Unavailable saved display — keep or choose another' });
                const signature = JSON.stringify([targets, targetValue]);
                if (signature !== targetChoicesSignature) {
                    target.setChoices(targets, targetValue);
                    targetChoicesSignature = signature;
                }
                const selectedTarget = draft.target.kind === 'existing' ? editor.targets.find(current => current.selector === draft.target.selector) : null;
                targetStatus.textContent = draft.target.kind === 'owned' ? 'A separate monitor is retained for the paired client.'
                    : selectedTarget ? `${selectedTarget.label}: ${selectedTarget.ready ? 'ready' : 'unavailable'}${selectedTarget.error ? ` — ${selectedTarget.error}` : ''}.`
                        : 'The saved display is currently unavailable. This profile keeps its target and will not capture a different display.';
                targetStatus.dataset.error = String(Boolean(editor.targetError) || draft.target.kind === 'existing' && !selectedTarget?.ready);
                if (editor.targetError)
                    targetStatus.textContent += ` ${editor.targetError}`;
            }
            else
                targetStatus.textContent = editor.targetError;
            const invalid = editor.validationError();
            name.setState({ error: draft && (!draft.name.trim() || new TextEncoder().encode(draft.name).length > 160) ? 'Enter a shorter, nonempty profile name.' : null });
            save.setState({ disabled: disabled || !editor.dirty() || invalid !== null || editor.confirmationRequired, busy: editor.busy });
            create.setState({ disabled: editor.busy || !canWrite || !saved || editor.dirty() || saved.profiles.length >= 16 });
            refresh.setState({ disabled: editor.busy });
            discard.setState({ disabled: editor.busy || !editor.dirty() });
            remove.setState({ disabled: editor.busy || !canDelete || !draft?.id || editor.dirty() || editor.confirmationRequired });
            notice.textContent = editor.message || (editor.dirty() && invalid ? invalid : '');
            notice.dataset.error = String(editor.error || editor.dirty() && invalid !== null);
            draftHint.textContent = editor.dirty() ? 'Save or discard these changes before switching profiles.' : '';
            for (const action of dialogActions)
                action.setState({ disabled: editor.busy });
            guard.update(editor.dirty(), editor.busy);
        }
        const dispose = () => {
            if (disposed)
                return;
            disposed = true;
            editor.dispose();
            closeDialog();
            for (const control of controls)
                control.dispose();
            for (const action of actions)
                action.dispose();
            card.dispose();
            guard.dispose();
            sdk.signal.removeEventListener('abort', dispose);
        };
        sdk.signal.addEventListener('abort', dispose, { once: true });
        void editor.refresh().then(async () => { if (!disposed && canReadTargets)
            await editor.refreshTargets(); });
        return dispose;
    }
    RemoteSessionUi.mountProfiles = mountProfiles;
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
        const childEditors = [];
        const canRead = sdk.canCall?.('preferences') !== false;
        const canWrite = sdk.canCall?.('preferences_update') === true;
        const guards = new RemoteSessionUi.EditorGuards((dirty, busy) => {
            sdk.setDirty?.(dirty);
            sdk.setBusy?.(busy);
        });
        const settingsGuard = guards.create();
        const editor = new RemoteSessionUi.SettingsEditor(sdk, render);
        const dispose = () => {
            if (disposed)
                return;
            disposed = true;
            editor.dispose();
            settingsGuard.dispose();
            for (const cleanup of childEditors)
                cleanup();
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
            settingsGuard.update(editor.dirty(), editor.busy);
        }
        const load = async () => {
            try {
                await sdk.assets?.stylesheet('remote-session.css');
                if (disposed || sdk.signal.aborted)
                    return;
                if (!canRead)
                    throw new Error('Your account cannot read remote session settings.');
                await editor.refresh();
                if (!disposed && !sdk.signal.aborted) {
                    childEditors.push(RemoteSessionUi.mountLayout(root, sdk, guards.create()));
                    childEditors.push(RemoteSessionUi.mountProfiles(root, sdk, guards.create()));
                }
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
