namespace RemoteSessionUi {
  export const booleanSettings = [
    'show_remote_input',
    'show_remote_monitor',
    'remote_monitor_mute_audio',
    'remote_monitor_disconnect_on_stream_end',
    'remote_monitor_disconnect_on_client_disconnect',
    'remote_monitor_terminate_on_first_request',
    'remote_monitor_confirm_app_replacement',
  ] as const;

  export type BooleanSetting = typeof booleanSettings[number];
  export type ProductSettings = Record<BooleanSetting, boolean> & { max_clients: number };
  export interface SettingsReply {
    ok: true;
    settings: ProductSettings;
    revision: number;
  }
  export interface SettingsApi {
    call(alias: string, payload?: Record<string, unknown>): Promise<unknown>;
  }

  function object(value: unknown): Record<string, unknown> | null {
    return value !== null && typeof value === 'object' && !Array.isArray(value)
      ? value as Record<string, unknown>
      : null;
  }

  export function parseSettings(value: unknown): ProductSettings {
    const source = object(value);
    if (!source) throw new Error('Remote session settings are invalid.');
    const result = {} as ProductSettings;
    for (const name of booleanSettings) {
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

  export function settingsEqual(left: ProductSettings, right: ProductSettings): boolean {
    return booleanSettings.every(name => left[name] === right[name])
      && left.max_clients === right.max_clients;
  }

  export function requestError(value: unknown): Error | null {
    const reply = object(value);
    if (reply?.ok !== false) return null;
    const message = reply.code === 'revision_conflict'
      ? 'Settings changed in another editor. Refresh saved settings to keep your draft and try again.'
      : typeof reply.message === 'string' ? reply.message : 'The settings request was rejected.';
    return Object.assign(new Error(message), { code: reply.code });
  }

  export function parseReply(value: unknown): SettingsReply {
    const error = requestError(value);
    if (error) throw error;
    const reply = object(value);
    if (reply?.ok !== true) throw new Error('The settings service did not confirm success.');
    if (typeof reply.revision !== 'number' || !Number.isSafeInteger(reply.revision)
      || reply.revision < 0) {
      throw new Error('The settings revision cannot be represented safely in this editor.');
    }
    return { ok: true, revision: reply.revision, settings: parseSettings(reply.settings) };
  }

  export class SettingsEditor {
    saved: SettingsReply | null = null;
    draft: ProductSettings | null = null;
    busy = false;
    message = '';
    error = false;
    confirmationRequired = false;
    private disposed = false;
    private generation = 0;

    constructor(private readonly api: SettingsApi, private readonly changed: () => void) {}

    dirty(): boolean {
      return this.saved !== null && this.draft !== null
        && !settingsEqual(this.saved.settings, this.draft);
    }

    valid(): boolean {
      try { parseSettings(this.draft); return this.draft !== null; }
      catch { return false; }
    }

    edit(name: BooleanSetting, value: boolean): void;
    edit(name: 'max_clients', value: number): void;
    edit(name: BooleanSetting | 'max_clients', value: boolean | number): void {
      if (this.disposed || this.busy || this.draft === null) return;
      this.draft = { ...this.draft, [name]: value };
      this.message = '';
      this.error = false;
      this.changed();
    }

    async refresh(discardDraft = false): Promise<void> {
      if (this.disposed || this.busy) return;
      const generation = ++this.generation;
      this.busy = true;
      this.message = 'Reading saved settings…';
      this.error = false;
      this.changed();
      try {
        const current = parseReply(await this.api.call('preferences'));
        if (!this.active(generation)) return;
        const previous = this.saved;
        const draft = this.draft;
        const refreshed = { ...current.settings };
        if (!discardDraft && previous !== null && draft !== null) {
          for (const name of booleanSettings) {
            if (draft[name] !== previous.settings[name]) refreshed[name] = draft[name];
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
      } catch (error) {
        if (!this.active(generation)) return;
        this.message = errorMessage(error);
        this.error = true;
      } finally {
        if (this.active(generation)) {
          this.busy = false;
          this.changed();
        }
      }
    }

    async save(): Promise<void> {
      if (this.disposed || this.busy || !this.saved || !this.draft
        || !this.dirty() || this.confirmationRequired) return;
      let requested: ProductSettings;
      try { requested = parseSettings(this.draft); }
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
        if (!this.active(generation)) return;
        const rejection = requestError(response);
        writeRejected = rejection !== null;
        if (rejection) throw rejection;
        const updated = parseReply(response);
        if (updated.revision !== revision + 1 || !settingsEqual(updated.settings, requested)) {
          throw new Error('The update did not confirm the requested settings. Refresh saved settings to check its outcome.');
        }
        const readback = parseReply(await this.api.call('preferences'));
        if (!this.active(generation)) return;
        if (readback.revision !== updated.revision || !settingsEqual(readback.settings, requested)) {
          throw new Error('Settings changed before the save could be confirmed. Refresh saved settings to check its outcome.');
        }
        this.saved = readback;
        this.draft = { ...readback.settings };
        this.message = 'Settings saved and confirmed.';
        this.confirmationRequired = false;
      } catch (error) {
        if (!this.active(generation)) return;
        this.message = errorMessage(error);
        this.error = true;
        this.confirmationRequired = !writeRejected;
        if (this.confirmationRequired) {
          this.message += ' Refresh saved settings before saving again; your draft is kept.';
        }
      } finally {
        if (this.active(generation)) {
          this.busy = false;
          this.changed();
        }
      }
    }

    dispose(): void {
      this.disposed = true;
      this.generation += 1;
    }

    private active(generation: number): boolean {
      return !this.disposed && this.generation === generation;
    }
  }

  export function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : 'The settings request failed.';
  }
}
