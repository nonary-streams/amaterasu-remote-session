namespace RemoteSessionUi {
  type Sdk = AmaterasuPluginUi.ExtensionPageSdk;
  type Field<T> = AmaterasuPluginUi.ExtensionUiControl<T>;
  type Action = AmaterasuPluginUi.ExtensionUiActionControl;

  const descriptions: Record<BooleanSetting, { label: string; help: string; group: 'tiles' | 'monitor' | 'safety' }> = {
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

  function text(tag: string, content: string): HTMLElement {
    const node = document.createElement(tag);
    node.textContent = content;
    return node;
  }

  export function mount(container: HTMLElement, sdk: Sdk): () => void {
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
    const cards = new Map<string, AmaterasuPluginUi.ExtensionUiCard>();
    const fields = new Map<BooleanSetting, Field<boolean>>();
    const actions: Action[] = [];
    let limit: Field<number> | null = null;
    let save: Action | null = null;
    let refresh: Action | null = null;
    let discard: Action | null = null;
    let disposed = false;
    let initialized = false;
    const childEditors: (() => void)[] = [];
    const canRead = sdk.canCall?.('preferences') !== false;
    const canWrite = sdk.canCall?.('preferences_update') === true;
    const guards = new EditorGuards((dirty, busy) => {
      sdk.setDirty?.(dirty);
      sdk.setBusy?.(busy);
    });
    const settingsGuard = guards.create();
    const editor = new SettingsEditor(sdk, render);

    const dispose = () => {
      if (disposed) return;
      disposed = true;
      editor.dispose();
      settingsGuard.dispose();
      for (const cleanup of childEditors) cleanup();
      for (const field of fields.values()) field.dispose();
      limit?.dispose();
      for (const action of actions) action.dispose();
      for (const card of cards.values()) card.dispose();
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
      const card = ui.card({ label: label! });
      card.content.classList.add('remote-session-fields');
      cards.set(id!, card);
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

    function initialize(): void {
      if (!ui || !editor.draft || initialized) return;
      initialized = true;
      for (const name of booleanSettings) {
        const description = descriptions[name];
        const field = ui.toggle({
          label: description.label, help: description.help, value: editor.draft[name],
          disabled: !canWrite,
          onChange: value => editor.edit(name, value),
        });
        fields.set(name, field);
        cards.get(description.group)!.append(field.element);
      }
      limit = ui.numberField({
        label: 'Maximum retained clients', value: editor.draft.max_clients,
        minimum: 1, maximum: 32, step: 1, required: true, disabled: !canWrite,
        help: 'Limit the paired clients with retained Remote Monitor or Remote Input sessions. Use a whole number from 1 to 32.',
        onChange: value => editor.edit('max_clients', value),
      });
      cards.get('monitor')!.append(limit.element);

    }

    function render(): void {
      if (disposed || sdk.signal.aborted) return;
      initialize();
      if (editor.draft) {
        for (const [name, field] of fields) {
          if (field.value() !== editor.draft[name]) field.setValue(editor.draft[name]);
          field.setState({ disabled: editor.busy || !canWrite });
        }
        if (limit) {
          if (!Object.is(limit.value(), editor.draft.max_clients)) limit.setValue(editor.draft.max_clients);
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
        if (disposed || sdk.signal.aborted) return;
        if (!canRead) throw new Error('Your account cannot read remote session settings.');
        await editor.refresh();
        if (!disposed && !sdk.signal.aborted) {
          childEditors.push(mountLayout(root, sdk, guards.create()));
          childEditors.push(mountProfiles(root, sdk, guards.create()));
        }
      } catch (error) {
        if (!disposed && !sdk.signal.aborted) {
          notice.textContent = errorMessage(error);
          notice.dataset.error = 'true';
        }
      }
    };
    void load();
    return dispose;
  }

  window.AmaterasuExtensions.v1.register('remote-session', {
    title: 'Remote sessions',
    mountSettings: mount,
  });
}
