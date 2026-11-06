namespace RemoteSessionUi {
  export type MonitorTarget = { kind: 'owned' } | { kind: 'existing'; selector: string };
  export interface MonitorProfile {
    id: string;
    uuid: string;
    name: string;
    width: number;
    height: number;
    refresh_millihz: number;
    hdr_enabled: boolean;
    scale: number;
    target: MonitorTarget;
  }
  export interface ProfilesReply { ok: true; revision: number; profiles: MonitorProfile[] }
  export interface ProfileCursor { revision: number; after: string }
  export interface ProfilesPage extends ProfilesReply { next_cursor: ProfileCursor | null }
  export const profileReadBounds = Object.freeze({ pageSize: 32, pages: 100_000,
    entries: 100_000, pageBytes: 12_000, totalBytes: 128 * 1024 * 1024 });
  export interface ProfileTarget {
    selector: string; label: string; ready: boolean; error?: string;
    mode?: { width: number; height: number; refresh_millihz: number; hdr_enabled: boolean };
  }
  export const profileScales = [-1, 0, 100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500] as const;

  export function newProfile(): MonitorProfile {
    return { id: '', uuid: '', name: '', width: 1920, height: 1080, refresh_millihz: 60_000,
      hdr_enabled: false, scale: 100, target: { kind: 'owned' } };
  }
  export function parseProfile(value: unknown, creating = false): MonitorProfile {
    const source = managementObject(value);
    const id = managementText(source.id, 24, creating);
    if (id !== '' && (!/^app-[0-9a-f]{16}$/u.test(id) || id === 'app-0000000000000000')) throw new Error('The profile identity is invalid.');
    const uuid = managementText(source.uuid, 36, creating);
    if (uuid !== '' && (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/iu.test(uuid)
      || uuid === '00000000-0000-0000-0000-000000000000')) throw new Error('The profile identity is invalid.');
    if (creating && (id !== '' || uuid !== '')) throw new Error('New profile identities must be assigned by the catalog.');
    if (typeof source.hdr_enabled !== 'boolean') throw new Error('Choose whether the profile uses HDR.');
    const scale = managementInteger(source.scale, -1, 500, 'Choose a supported display scale.');
    if (!(profileScales as readonly number[]).includes(scale)) throw new Error('Choose a supported display scale.');
    const targetSource = source.target === undefined ? { kind: 'owned' } : managementObject(source.target);
    let target: MonitorTarget;
    if (targetSource.kind === 'owned') target = { kind: 'owned' };
    else if (targetSource.kind === 'existing') target = { kind: 'existing', selector: managementText(targetSource.selector, 256) };
    else throw new Error('Choose a supported display target.');
    return { id, uuid, name: managementText(source.name, 160),
      width: managementInteger(source.width, 320, 7680, 'Width must be a whole number from 320 to 7680 pixels.'),
      height: managementInteger(source.height, 200, 4320, 'Height must be a whole number from 200 to 4320 pixels.'),
      refresh_millihz: managementInteger(source.refresh_millihz, 1000, 1_000_000, 'Refresh rate must be from 1 to 1000 Hz, with up to three decimal places.'),
      hdr_enabled: source.hdr_enabled, scale, target };
  }
  function profilePageBytes(value: unknown): number {
    const serialized = JSON.stringify(value);
    if (serialized === undefined) throw new Error('The profile list response is invalid.');
    return new TextEncoder().encode(serialized).length;
  }
  export function parseProfilesReply(value: unknown): ProfilesPage {
    const error = requestError(value);
    if (error) throw error;
    const source = managementObject(value);
    if (source.ok !== true || !Array.isArray(source.profiles) || source.profiles.length > profileReadBounds.pageSize
      || profilePageBytes(value) > profileReadBounds.pageBytes) throw new Error('The profile page is unavailable or exceeds its limit.');
    if (!Object.hasOwn(source, 'next_cursor')) throw new Error('The profile page is missing its continuation field.');
    const profiles = source.profiles.map(profile => parseProfile(profile));
    if (new Set(profiles.map(profile => profile.id)).size !== profiles.length
      || new Set(profiles.map(profile => profile.uuid.toLowerCase())).size !== profiles.length) throw new Error('The profile list contains duplicate identities.');
    const revision = managementRevision(source.revision);
    let next_cursor: ProfileCursor | null = null;
    if (source.next_cursor !== null) {
      const cursor = managementObject(source.next_cursor);
      const after = managementText(cursor.after, 20);
      if (!/^app-[0-9a-f]{16}$/u.test(after) || after === 'app-0000000000000000' || managementRevision(cursor.revision) !== revision) {
        throw new Error('The profile continuation cursor is invalid.');
      }
      next_cursor = { revision, after };
    }
    return { ok: true, revision, profiles, next_cursor };
  }

  /** Publish a complete revision; empty pages can advance over ordinary catalog rows. */
  export async function readProfiles(api: SettingsApi, active: () => boolean = () => true): Promise<ProfilesReply> {
    const profiles: MonitorProfile[] = [];
    const identities = new Set<string>(), uuids = new Set<string>();
    let cursor: ProfileCursor | null = null;
    let revision: number | null = null;
    let lastId: string | null = null;
    let bytes = 0;
    for (let pageNumber = 0; pageNumber < profileReadBounds.pages; pageNumber++) {
      if (!active()) throw new Error('The profile editor is closed.');
      const response = await api.call('profiles', { limit: profileReadBounds.pageSize, ...(cursor ? { cursor } : {}) });
      if (!active()) throw new Error('The profile editor is closed.');
      const page = parseProfilesReply(response);
      if (revision !== null && page.revision !== revision) throw new Error('The profile catalog changed while loading. Refresh profiles to try again.');
      revision = page.revision;
      bytes += profilePageBytes(response);
      if (profiles.length + page.profiles.length > profileReadBounds.entries || bytes > profileReadBounds.totalBytes) {
        throw new Error('The complete profile catalog exceeds this editor’s capacity. No partial list was loaded.');
      }
      for (const profile of page.profiles) {
        if (identities.has(profile.id) || uuids.has(profile.uuid.toLowerCase())) throw new Error('Profile pages contain duplicate identities.');
        if (lastId !== null && profile.id <= lastId || cursor !== null && profile.id <= cursor.after) {
          throw new Error('Profile pages are duplicated or out of order.');
        }
        identities.add(profile.id); uuids.add(profile.uuid.toLowerCase()); lastId = profile.id;
        profiles.push(profile);
      }
      if (page.next_cursor === null) return { ok: true, revision, profiles };
      if (cursor !== null && page.next_cursor.after <= cursor.after
        || lastId !== null && page.next_cursor.after < lastId) throw new Error('The profile continuation cursor did not advance.');
      cursor = page.next_cursor;
    }
    throw new Error('The profile catalog exceeds the pagination limit. No partial list was loaded.');
  }
  export function parseProfileTargets(value: unknown): ProfileTarget[] {
    const error = requestError(value);
    if (error) throw error;
    const source = managementObject(value);
    if (source.ok !== true || !Array.isArray(source.targets) || source.targets.length > 32) throw new Error('The display target list is unavailable.');
    const targets = source.targets.map(value => {
      const target = managementObject(value);
      if (typeof target.ready !== 'boolean') throw new Error('The target readiness report is invalid.');
      let mode: ProfileTarget['mode'];
      if (target.mode !== undefined) {
        const source = managementObject(target.mode);
        if (typeof source.hdr_enabled !== 'boolean') throw new Error('The observed display mode is invalid.');
        mode = { width: managementInteger(source.width, 1, 65_535, 'The observed display width is invalid.'),
          height: managementInteger(source.height, 1, 65_535, 'The observed display height is invalid.'),
          refresh_millihz: managementInteger(source.refresh_millihz, 1, 2_000_000, 'The observed display refresh rate is invalid.'),
          hdr_enabled: source.hdr_enabled };
      }
      return { selector: managementText(target.selector, 256), label: managementText(target.label, 160), ready: target.ready,
        ...(mode === undefined ? {} : { mode }),
        ...(target.error === undefined ? {} : { error: managementText(target.error, 1024) }) };
    });
    if (new Set(targets.map(target => target.selector)).size !== targets.length) throw new Error('The target list contains duplicate identities.');
    return targets;
  }
  export function profileEqual(left: MonitorProfile, right: MonitorProfile): boolean {
    return JSON.stringify(left) === JSON.stringify(right);
  }

  export class ProfileEditor {
    saved: ProfilesReply | null = null;
    draft: MonitorProfile | null = null;
    targets: ProfileTarget[] = [];
    targetError = '';
    busy = false;
    error = false;
    message = '';
    confirmationRequired = false;
    private disposed = false;
    private generation = 0;
    private original: MonitorProfile | null = null;
    private creationBaseline: Set<string> | null = null;

    constructor(private readonly api: SettingsApi, private readonly changed: () => void) {}
    dirty(): boolean {
      return this.draft !== null && (this.original === null || !profileEqual(this.draft, this.original));
    }
    select(id: string): void {
      if (this.disposed || this.busy || this.dirty() || !this.saved) return;
      const profile = this.saved.profiles.find(profile => profile.id === id);
      this.original = profile ? { ...profile, target: { ...profile.target } } : null;
      this.draft = profile ? { ...profile, target: { ...profile.target } } : newProfile();
      this.error = false; this.message = ''; this.changed();
    }
    edit(patch: Partial<Pick<MonitorProfile, 'name' | 'width' | 'height' | 'refresh_millihz' | 'hdr_enabled' | 'scale' | 'target'>>): void {
      if (this.disposed || this.busy || !this.draft) return;
      this.draft = { ...this.draft, ...patch };
      this.error = false; this.message = ''; this.changed();
    }
    validationError(): string | null {
      try {
        if (!this.draft) return 'Choose a profile to edit.';
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
      } catch (error) { return errorMessage(error); }
    }
    async refresh(discardDraft = false): Promise<void> {
      if (this.disposed || this.busy) return;
      const generation = ++this.generation;
      this.busy = true; this.error = false; this.message = 'Reading monitor profiles…'; this.changed();
      try {
        const current = await readProfiles(this.api, () => this.active(generation));
        if (!this.active(generation)) return;
        let draft = this.draft;
        let keep = !discardDraft && this.dirty();
        let ambiguousCreate = false;
        if (this.confirmationRequired && draft?.id === '' && this.creationBaseline !== null) {
          const requested = draft;
          const matches = current.profiles.filter(profile => !this.creationBaseline!.has(profile.id)
            && profileEqual({ ...profile, id: '', uuid: '' }, requested));
          if (matches.length === 1) { draft = matches[0]!; keep = false; }
          else if (matches.length > 1) ambiguousCreate = true;
        }
        this.saved = current;
        if (keep && draft) {
          const previous = this.original;
          this.original = current.profiles.find(profile => profile.id === draft.id) ?? null;
          if (previous && this.original) {
            const merged = { ...this.original };
            for (const name of ['name', 'width', 'height', 'refresh_millihz', 'hdr_enabled', 'scale', 'target'] as const) {
              if (JSON.stringify(previous[name]) !== JSON.stringify(draft[name])) {
                Object.assign(merged, { [name]: draft[name] });
              }
            }
            this.draft = merged;
          } else this.draft = draft;
        } else {
          const profile = current.profiles.find(profile => profile.id === draft?.id) ?? current.profiles[0] ?? null;
          this.original = profile;
          this.draft = profile ? { ...profile, target: { ...profile.target } } : null;
        }
        this.confirmationRequired = ambiguousCreate;
        if (!ambiguousCreate) this.creationBaseline = null;
        this.message = ambiguousCreate ? 'More than one matching saved profile exists. Discard this draft and select the intended profile before making another change.'
          : this.dirty() ? 'Profiles refreshed. Your unsaved changes are kept.' : 'Monitor profiles are up to date.';
        this.error = ambiguousCreate;
      } catch (error) {
        if (this.active(generation)) { this.error = true; this.message = errorMessage(error); }
      } finally { if (this.active(generation)) { this.busy = false; this.changed(); } }
    }
    async refreshTargets(): Promise<void> {
      if (this.disposed || this.busy) return;
      const generation = this.generation;
      try {
        const targets = parseProfileTargets(await this.api.call('profile_targets'));
        if (!this.active(generation)) return;
        this.targets = targets; this.targetError = ''; this.changed();
      } catch (error) {
        if (this.active(generation)) { this.targetError = errorMessage(error); this.changed(); }
      }
    }
    async save(): Promise<void> {
      if (this.disposed || this.busy || !this.saved || !this.draft || !this.dirty() || this.confirmationRequired) return;
      const invalid = this.validationError();
      if (invalid) { this.error = true; this.message = invalid; this.changed(); return; }
      const requested = parseProfile(this.draft, this.draft.id === ''), revision = this.saved.revision;
      const generation = ++this.generation;
      this.creationBaseline = requested.id === '' ? new Set(this.saved.profiles.map(profile => profile.id)) : null;
      this.busy = true; this.error = false; this.message = 'Saving monitor profile…'; this.changed();
      let rejected = false;
      try {
        const response = await this.api.call('profile_upsert', { profile: requested, expected_revision: revision });
        if (!this.active(generation)) return;
        const rejection = requestError(response); rejected = rejection !== null;
        if (rejection) { this.creationBaseline = null; throw rejection; }
        const updated = managementObject(response);
        if (updated.ok !== true) throw new Error('The profile service did not confirm the update.');
        const confirmed = parseProfile(updated.profile), updatedRevision = managementRevision(updated.revision);
        if (updatedRevision <= revision || (requested.id !== '' && (confirmed.id !== requested.id || confirmed.uuid !== requested.uuid))
          || !profileEqual({ ...confirmed, id: requested.id, uuid: requested.uuid }, requested)) throw new Error('The profile update did not confirm the requested values.');
        const readback = await readProfiles(this.api, () => this.active(generation));
        if (!this.active(generation)) return;
        const profile = readback.profiles.find(profile => profile.id === confirmed.id);
        if (readback.revision !== updatedRevision || !profile || !profileEqual(profile, confirmed)) throw new Error('The saved profile changed before readback.');
        this.saved = readback; this.original = profile; this.draft = { ...profile, target: { ...profile.target } };
        this.confirmationRequired = false; this.creationBaseline = null; this.message = 'Monitor profile saved and confirmed.';
      } catch (error) {
        if (!this.active(generation)) return;
        this.error = true; this.message = errorMessage(error); this.confirmationRequired = !rejected;
        if (this.confirmationRequired) this.message += ' Refresh profiles before saving again; your draft is kept.';
      } finally { if (this.active(generation)) { this.busy = false; this.changed(); } }
    }
    async remove(): Promise<void> {
      if (this.disposed || this.busy || this.dirty() || !this.saved || !this.draft || this.draft.id === '' || this.confirmationRequired) return;
      const id = this.draft.id, name = this.draft.name, revision = this.saved.revision, generation = ++this.generation;
      this.busy = true; this.error = false; this.message = 'Deleting monitor profile…'; this.changed();
      let rejected = false;
      try {
        const response = await this.api.call('profile_delete', { id, expected_revision: revision });
        if (!this.active(generation)) return;
        const rejection = requestError(response); rejected = rejection !== null;
        if (rejection) throw rejection;
        const updated = managementObject(response), updatedRevision = managementRevision(updated.revision);
        if (updated.ok !== true || updatedRevision <= revision) throw new Error('The profile service did not confirm deletion.');
        const readback = await readProfiles(this.api, () => this.active(generation));
        if (!this.active(generation)) return;
        if (readback.revision !== updatedRevision || readback.profiles.some(profile => profile.id === id)) throw new Error('The deleted profile is still present in readback.');
        this.saved = readback; this.original = readback.profiles[0] ?? null;
        this.draft = this.original ? { ...this.original, target: { ...this.original.target } } : null;
        this.confirmationRequired = false; this.message = `Monitor profile “${name}” deleted and confirmed. Retained monitors keep their accepted settings.`;
      } catch (error) {
        if (!this.active(generation)) return;
        this.error = true; this.message = errorMessage(error); this.confirmationRequired = !rejected;
        if (this.confirmationRequired) this.message += ' Refresh profiles to check the deletion before trying again.';
      } finally { if (this.active(generation)) { this.busy = false; this.changed(); } }
    }
    dispose(): void { this.disposed = true; this.generation++; }
    private active(generation: number): boolean { return !this.disposed && generation === this.generation; }
  }
}
