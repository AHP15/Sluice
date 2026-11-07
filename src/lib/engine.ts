import { ENGINES, type Engine, type ProfileMeta } from "./contract";

/**
 * T-075: switch a profile's engine. A port still at the old engine's default
 * follows to the new one's; a port the user chose is kept.
 */
export function withEngine(form: ProfileMeta, engine: Engine): ProfileMeta {
  const was = ENGINES.find((e) => e.id === (form.engine ?? "postgres"));
  const now = ENGINES.find((e) => e.id === engine);
  const port = was && now && form.port === was.port ? now.port : form.port;
  return { ...form, engine, port };
}
