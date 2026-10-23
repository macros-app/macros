import type { Metadata, Viewport } from "next";
import "./globals.css";
import { plotFont } from "./_spectrum/plotFont";

const DESCRIPTION =
  "Macros is an Emacs-style, GPU-rendered code editor written in Rust and scripted in Steel (Scheme). Spreadsheet buffers, charts drawn in text, Magit, Org mode, LSP, and Emacs, evil, or helix keys — every keybinding and command is yours to redefine.";

// Rendered at build time by app/og.png/route.tsx.
const OG_IMAGE = {
  url: "/og.png",
  width: 1200,
  height: 630,
  alt: "Macros — Emacs ideas, a modern engine. An Emacs-style editor in Rust, scripted in Steel Scheme.",
};

export const metadata: Metadata = {
  metadataBase: new URL("https://macros.dev"),
  title: {
    default: "Macros — Emacs ideas, a modern engine",
    template: "%s — Macros",
  },
  description: DESCRIPTION,
  applicationName: "Macros",
  keywords: [
    "Macros",
    "text editor",
    "code editor",
    "Emacs",
    "Steel",
    "Scheme",
    "Lisp",
    "modal editing",
    "vim",
    "evil-mode",
    "LSP",
    "magit",
    "GPUI",
    "GPU rendered",
    "macOS",
    "Windows",
    "Linux",
  ],
  authors: [{ name: "The Macros team" }],
  openGraph: {
    type: "website",
    siteName: "Macros",
    title: "Macros — Emacs ideas, a modern engine",
    description: DESCRIPTION,
    url: "https://macros.dev",
    images: [OG_IMAGE],
  },
  twitter: {
    card: "summary_large_image",
    title: "Macros — Emacs ideas, a modern engine",
    description: DESCRIPTION,
    images: [OG_IMAGE],
  },
  icons: {
    icon: "/favicon-32.png",
    apple: "/brand/icon-256.png",
  },
};

export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
  themeColor: "#0b0a10",
  colorScheme: "dark",
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="en" className={plotFont.variable}>
      <body>{children}</body>
    </html>
  );
}
