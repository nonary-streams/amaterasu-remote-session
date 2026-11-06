namespace RemoteSessionUi {
  function monitorText(tag: string, content: string, className = ''): HTMLElement {
    const element = document.createElement(tag);
    element.textContent = content; element.className = className;
    return element;
  }
  function placementMode(placement: MonitorPlacement): string {
    return !placement.preserve ? 'relative' : placement.automatic_fallback ? 'automatic' : 'keep';
  }
  export function mountLayout(container: HTMLElement, sdk: AmaterasuPluginUi.ExtensionPageSdk, guard: EditorGuard): () => void {
    const ui = sdk.ui;
    if (!ui || !sdk.canCall?.('layout')) { guard.dispose(); return () => {}; }
    const card = ui.card({ label: 'Monitor positions' });
    card.content.classList.add('remote-session-fields');
    card.element.classList.add('remote-session-layout');
    const description = monitorText('p', 'Choose where each client’s monitor appears beside host displays or other clients. Active monitors may need to reconnect before a saved position takes effect.');
    const notice = monitorText('p', 'Reading saved positions…', 'remote-session-notice');
    notice.setAttribute('role', 'status'); notice.setAttribute('aria-live', 'polite');
    const readiness = monitorText('p', '', 'remote-session-readiness');
    const application = monitorText('p', '', 'remote-session-readiness');
    const preview = monitorText('p', '', 'remote-session-layout-preview');
    card.append(description); card.append(notice);
    const fields: AmaterasuPluginUi.ExtensionUiControl<unknown>[] = [];
    const actions: AmaterasuPluginUi.ExtensionUiActionControl[] = [];
    let selected: string | null = null;
    let clientChoicesSignature = '', anchorChoicesSignature = '';
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const canWrite = sdk.canCall('layout_update');
    const editor = new LayoutEditor(sdk, render);
    const client = ui.select({ label: 'Client monitor', choices: [{ value: '__none__', label: 'No clients available' }],
      value: '__none__', disabled: true, onChange: value => { selected = value === '__none__' ? null : value.slice(7); render(); } });
    const mode = ui.select({ label: 'Position', choices: [
      { value: 'automatic', label: 'Automatic — append to the right' },
      { value: 'relative', label: 'Beside a display or client' },
      { value: 'keep', label: 'Keep its current position' },
    ], value: 'automatic', disabled: true, onChange: value => {
      if (!selected || !editor.draft) return;
      const current = editor.draft.placements[selected] ?? automaticPlacement();
      if (value === 'keep') editor.edit(selected, { ...automaticPlacement(), automatic_fallback: false });
      else if (value === 'automatic') editor.edit(selected, { ...automaticPlacement(), primary: current.primary });
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
        if (value !== '__none__') edit({ anchor_kind: value.startsWith('client:') ? 'client' : 'physical',
          anchor_id: value.startsWith('client:') ? value.slice(7) : value.slice(9) });
      } });
    const edge = ui.select({ label: 'Place monitor', choices: [
      { value: 'left', label: 'To the left of the anchor' }, { value: 'right', label: 'To the right of the anchor' },
      { value: 'above', label: 'Above the anchor' }, { value: 'below', label: 'Below the anchor' },
    ], value: 'right', disabled: true, onChange: value => edit({ edge: value as MonitorEdge }) });
    const alignment = ui.select({ label: 'Align edges', choices: [
      { value: 'start', label: 'Start — top or left' }, { value: 'center', label: 'Center' }, { value: 'end', label: 'End — bottom or right' },
    ], value: 'start', disabled: true, onChange: value => edit({ alignment: value as MonitorAlignment }) });
    const gap = ui.numberField({ label: 'Gap between monitors (pixels)', value: 0, minimum: 0, maximum: 10_000, step: 1,
      disabled: true, onChange: value => edit({ gap_px: value }) });
    const primary = ui.toggle({ label: 'Use this monitor as primary', value: false, disabled: true,
      help: 'Only one saved client monitor can be primary. A current-position rule leaves the existing primary unchanged.',
      onChange: value => edit({ primary: value }) });
    fields.push(client, mode, anchor, edge, alignment, gap, primary);
    for (const field of fields) card.append(field.element);
    card.append(readiness); card.append(preview); card.append(application);
    const buttons = document.createElement('div'); buttons.className = 'remote-session-actions';
    const save = ui.button({ label: 'Save monitor positions', variant: 'primary', disabled: true, onClick: () => editor.save() });
    const refresh = ui.button({ label: 'Refresh saved positions', onClick: () => editor.refresh() });
    const discard = ui.button({ label: 'Discard position changes', disabled: true, onClick: () => editor.refresh(true) });
    const clear = ui.button({ label: 'Clear this client’s saved rule', disabled: true,
      onClick: () => { if (selected) editor.edit(selected, null); } });
    actions.push(save, refresh, discard, clear);
    buttons.append(...actions.map(action => action.element)); card.append(buttons);
    container.append(card.element);

    function edit(patch: Partial<MonitorPlacement>): void {
      if (!selected || !editor.draft) return;
      editor.edit(selected, { ...(editor.draft.placements[selected] ?? automaticPlacement()), ...patch });
    }
    function anchorChoices(): AmaterasuPluginUi.ExtensionUiOption[] {
      if (!editor.saved) return [];
      return [
        ...editor.saved.physical_anchors.map(anchor => ({ value: `physical:${anchor.id}`, label: `Host display — ${anchor.label}` })),
        ...editor.saved.clients.filter(client => client.id !== selected).map(client => ({ value: `client:${client.id}`, label: `Remote client — ${client.label}` })),
      ];
    }
    function render(): void {
      if (disposed || sdk.signal.aborted) return;
      const saved = editor.saved, draft = editor.draft;
      const choices = saved?.clients.map(client => ({ value: `client:${client.id}`, label: client.label })) ?? [];
      if (draft) {
        for (const id of Object.keys(draft.placements)) {
          if (!saved?.clients.some(client => client.id === id)) choices.push({ value: `client:${id}`, label: 'Unavailable saved client' });
        }
      }
      if (!choices.some(choice => choice.value === `client:${selected}`)) selected = choices[0]?.value.slice(7) ?? null;
      const clientChoices = choices.length ? choices : [{ value: '__none__', label: 'No clients available' }];
      const clientValue = selected ? `client:${selected}` : '__none__';
      const clientSignature = JSON.stringify([clientChoices, clientValue]);
      if (clientSignature !== clientChoicesSignature) { client.setChoices(clientChoices, clientValue); clientChoicesSignature = clientSignature; }
      client.setState({ disabled: editor.busy || choices.length === 0 });
      const placement = selected && draft ? draft.placements[selected] ?? automaticPlacement() : automaticPlacement();
      const currentMode = placementMode(placement);
      mode.setValue(currentMode);
      const anchors = anchorChoices();
      const anchorValue = `${placement.anchor_kind}:${placement.anchor_id}`;
      if (placement.anchor_id && !anchors.some(choice => choice.value === anchorValue)) anchors.push({ value: anchorValue, label: 'Unavailable saved anchor — choose another' });
      const chosenAnchor = placement.anchor_id ? anchorValue : anchors[0]?.value ?? '__none__';
      const availableAnchors = anchors.length ? anchors : [{ value: '__none__', label: 'No anchor available' }];
      const anchorSignature = JSON.stringify([availableAnchors, chosenAnchor]);
      if (anchorSignature !== anchorChoicesSignature) { anchor.setChoices(availableAnchors, chosenAnchor); anchorChoicesSignature = anchorSignature; }
      edge.setValue(placement.edge); alignment.setValue(placement.alignment);
      if (!Object.is(gap.value(), placement.gap_px)) gap.setValue(placement.gap_px);
      primary.setValue(placement.primary);
      const disabled = editor.busy || !canWrite || !selected || !draft;
      mode.setState({ disabled });
      for (const field of [anchor, edge, alignment, gap]) field.setState({ disabled: disabled || currentMode !== 'relative' });
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
      } else preview.textContent = currentMode === 'keep' ? 'Position: leave the current desktop arrangement unchanged.'
        : `Position: append to the right of the desktop${placement.primary ? '; primary monitor' : ''}.`;
      const status = saved?.application_status;
      application.textContent = status ? `Placement status: ${status.state.replaceAll('_', ' ')}${status.error ? ` — ${status.error}` : ''}.` : '';
      application.dataset.error = String(Boolean(status?.error));
      guard.update(editor.dirty(), editor.busy);
    }
    const poll = async () => {
      if (disposed || sdk.signal.aborted) return;
      if (document.visibilityState !== 'hidden') await editor.pollInventory();
      if (!disposed && !sdk.signal.aborted) timer = setTimeout(() => { void poll(); }, 2000);
    };
    const dispose = () => {
      if (disposed) return;
      disposed = true; editor.dispose();
      if (timer !== null) clearTimeout(timer);
      for (const field of fields) field.dispose();
      for (const action of actions) action.dispose();
      card.dispose(); guard.dispose(); sdk.signal.removeEventListener('abort', dispose);
    };
    sdk.signal.addEventListener('abort', dispose, { once: true });
    void editor.refresh().then(() => { if (!disposed && !sdk.signal.aborted) timer = setTimeout(() => { void poll(); }, 2000); });
    return dispose;
  }
}
