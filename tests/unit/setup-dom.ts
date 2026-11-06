/**
 * T-070 / D-021: component tests run against happy-dom with fake data — no
 * Tauri IPC and no WebDriver. Registered once for every unit test; the static
 * markup tests are unaffected by a DOM being present.
 */
import { GlobalRegistrator } from "@happy-dom/global-registrator";

GlobalRegistrator.register();

const { afterEach } = await import("bun:test");
const { cleanup } = await import("@testing-library/react");
afterEach(() => cleanup());
