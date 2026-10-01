import type { Metadata } from "next";
import type { ReactNode } from "react";
import { PROJECT_NAME } from "@/lib/project";
import "./globals.css";

export const metadata: Metadata = { title: PROJECT_NAME };

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en">
      <body>
        <main>{children}</main>
      </body>
    </html>
  );
}
