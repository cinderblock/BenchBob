import { createRoot } from "react-dom/client";
import "./styles/base.css";
import "./styles/app.css";
import { Bob } from "./bob/Bob";
import { BobConnection, defaultBobUrl } from "./lib/connection";

// The bob serving this page stamps its build version into this meta tag; `?bob=` or
// VITE_BOB point the page at a different bob (development, or a page served elsewhere).
const pageBuild = document.querySelector<HTMLMetaElement>('meta[name="benchbob-build"]')?.content ?? null;
const connection = new BobConnection(defaultBobUrl());

createRoot(document.getElementById("app")!).render(<Bob connection={connection} pageBuild={pageBuild} nodeOnly={__MCU__} />);
