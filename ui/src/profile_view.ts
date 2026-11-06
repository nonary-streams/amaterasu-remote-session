namespace RemoteSessionUi {
  export function mountProfiles(container: HTMLElement, sdk: AmaterasuPluginUi.ExtensionPageSdk, guard: EditorGuard): () => void {
    const ui = sdk.ui;
    if (!ui || !sdk.canCall?.('profiles')) { guard.dispose(); return () => {}; }
    const card = ui.card({ label: 'Remote Monitor profiles' });
    card.content.classList.add('remote-session-fields');
    card.element.classList.add('remote-session-profiles');
    const paragraph = (content: string, className = '') => {
      const element = document.createElement('p'); element.textContent = content; element.className = className; return element;
    };
    const description = paragraph('Create named monitor tiles for paired clients. Changing or deleting a profile affects future connections; a retained monitor keeps the settings it accepted until it disconnects.');
    const notice = paragraph('Reading monitor profiles…', 'remote-session-notice');
    notice.setAttribute('role', 'status'); notice.setAttribute('aria-live', 'polite');
    const targetStatus = paragraph('', 'remote-session-readiness');
    const draftHint = paragraph('', 'remote-session-readiness');
    card.append(description); card.append(notice);
    const controls: AmaterasuPluginUi.ExtensionUiControl<unknown>[] = [];
    const actions: AmaterasuPluginUi.ExtensionUiActionControl[] = [];
    let disposed = false;
    let dialog: AmaterasuPluginUi.ExtensionUiDialog | null = null;
    let dialogActions: AmaterasuPluginUi.ExtensionUiActionControl[] = [];
    let profileChoicesSignature = '', targetChoicesSignature = '';
    const canWrite = sdk.canCall('profile_upsert'), canDelete = sdk.canCall('profile_delete');
    const canReadTargets = sdk.canCall('profile_targets');
    const editor = new ProfileEditor(sdk, render);
    const selector = ui.select({ label: 'Saved monitor profile', choices: [{ value: '__none__', label: 'No saved profiles' }],
      value: '__none__', disabled: true, onChange: value => editor.select(value === '__new__' ? '' : value.slice(8)) });
    const name = ui.textField({ label: 'Profile name', value: '', maxLength: 160, required: true, disabled: true,
      placeholder: 'For example: Drawing tablet', onChange: value => editor.edit({ name: value }) });
    const target = ui.select({ label: 'Display target', choices: [{ value: 'owned', label: 'Create a monitor for this client' }],
      value: 'owned', disabled: true, help: 'Existing displays are selected from the host’s verified inventory. Their identity is kept with the profile.',
      onChange: value => {
        if (value === 'owned') editor.edit({ target: { kind: 'owned' } });
        else {
          const selected = editor.targets.find(target => target.selector === value.slice(9));
          if (selected?.ready) editor.edit({ target: { kind: 'existing', selector: selected.selector } });
          else render();
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
    const scale = ui.select({ label: 'Display scale', choices: profileScales.map(value => ({
      value: String(value), label: value <= 0 ? value === -1 ? 'Automatic (provider default)' : 'Automatic' : `${value}%`,
    })), value: '100', disabled: true, onChange: value => editor.edit({ scale: Number(value) }) });
    controls.push(selector, name, target, width, height, refreshRate, hdr, scale);
    for (const control of controls) card.append(control.element);
    card.append(targetStatus); card.append(draftHint);
    const buttons = document.createElement('div'); buttons.className = 'remote-session-actions';
    const create = ui.button({ label: 'New monitor profile', disabled: true, onClick: () => editor.select('') });
    const save = ui.button({ label: 'Save monitor profile', variant: 'primary', disabled: true, onClick: () => editor.save() });
    const refresh = ui.button({ label: 'Refresh saved profiles', onClick: async () => {
      await editor.refresh(); if (canReadTargets) await editor.refreshTargets();
    } });
    const discard = ui.button({ label: 'Discard profile changes', disabled: true, onClick: () => editor.refresh(true) });
    const remove = ui.button({ label: 'Delete monitor profile', variant: 'danger', disabled: true, onClick: confirmDelete });
    actions.push(create, save, refresh, discard, remove);
    buttons.append(...actions.map(action => action.element)); card.append(buttons); container.append(card.element);

    function closeDialog(): void {
      const current = dialog; dialog = null; current?.dispose();
      for (const action of dialogActions) action.dispose();
      dialogActions = [];
    }
    function confirmDelete(): void {
      if (!editor.draft || editor.busy || editor.dirty() || !canDelete || dialog) return;
      const message = paragraph(`Delete “${editor.draft.name}”? Clients will no longer see this profile tile. A monitor already using it keeps its accepted settings until disconnected.`);
      const confirm = ui!.button({ label: 'Delete this profile', variant: 'danger', onClick: async () => {
        await editor.remove(); closeDialog();
      } });
      dialogActions = [confirm];
      dialog = ui!.dialog({ title: 'Delete monitor profile', content: [message, confirm.element],
        cancelLabel: 'Keep profile', onClose: closeDialog });
      card.append(dialog.element);
      dialog.open();
    }
    function render(): void {
      if (disposed || sdk.signal.aborted) return;
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
      if (draft?.id === '') choices.unshift({ value: '__new__', label: 'New unsaved profile' });
      if (draft?.id && !saved?.profiles.some(profile => profile.id === draft.id)) choices.push({ value: `profile:${draft.id}`, label: `${draft.name} — removed in another editor` });
      const value = draft ? draft.id === '' ? '__new__' : `profile:${draft.id}` : '__none__';
      if (choices.length === 0) choices.push({ value: '__none__', label: 'No saved profiles' });
      const choicesSignature = JSON.stringify([choices, value]);
      if (choicesSignature !== profileChoicesSignature) { selector.setChoices(choices, value); profileChoicesSignature = choicesSignature; }
      selector.setState({ disabled: editor.busy || editor.dirty() || !saved || saved.profiles.length === 0 });
      const disabled = editor.busy || !canWrite || !draft;
      for (const control of [name, target, width, height, refreshRate, hdr, scale]) control.setState({ disabled });
      if (draft) {
        if (name.value() !== draft.name) name.setValue(draft.name);
        if (!Object.is(width.value(), draft.width)) width.setValue(draft.width);
        if (!Object.is(height.value(), draft.height)) height.setValue(draft.height);
        if (!Object.is(refreshRate.value(), draft.refresh_millihz / 1000)) refreshRate.setValue(draft.refresh_millihz / 1000);
        hdr.setValue(draft.hdr_enabled);
        if (scale.value() !== String(draft.scale)) scale.setValue(String(draft.scale));
        const targets = [{ value: 'owned', label: 'Create a monitor for this client' },
          ...editor.targets.filter(target => target.ready).map(target => ({ value: `existing:${target.selector}`, label: `Existing display — ${target.label}` }))];
        const targetValue = draft.target.kind === 'owned' ? 'owned' : `existing:${draft.target.selector}`;
        if (!targets.some(target => target.value === targetValue)) targets.push({ value: targetValue, label: 'Unavailable saved display — keep or choose another' });
        const signature = JSON.stringify([targets, targetValue]);
        if (signature !== targetChoicesSignature) { target.setChoices(targets, targetValue); targetChoicesSignature = signature; }
        const selectedTarget = draft.target.kind === 'existing' ? editor.targets.find(current => current.selector === (draft.target as { kind: 'existing'; selector: string }).selector) : null;
        targetStatus.textContent = draft.target.kind === 'owned' ? 'A separate monitor is retained for the paired client.'
          : selectedTarget ? `${selectedTarget.label}: ${selectedTarget.ready ? 'ready' : 'unavailable'}${selectedTarget.error ? ` — ${selectedTarget.error}` : ''}.`
            : 'The saved display is currently unavailable. This profile keeps its target and will not capture a different display.';
        targetStatus.dataset.error = String(Boolean(editor.targetError) || draft.target.kind === 'existing' && !selectedTarget?.ready);
        if (editor.targetError) targetStatus.textContent += ` ${editor.targetError}`;
      } else targetStatus.textContent = editor.targetError;
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
      for (const action of dialogActions) action.setState({ disabled: editor.busy });
      guard.update(editor.dirty(), editor.busy);
    }
    const dispose = () => {
      if (disposed) return;
      disposed = true; editor.dispose(); closeDialog();
      for (const control of controls) control.dispose();
      for (const action of actions) action.dispose();
      card.dispose(); guard.dispose(); sdk.signal.removeEventListener('abort', dispose);
    };
    sdk.signal.addEventListener('abort', dispose, { once: true });
    void editor.refresh().then(async () => { if (!disposed && canReadTargets) await editor.refreshTargets(); });
    return dispose;
  }
}
