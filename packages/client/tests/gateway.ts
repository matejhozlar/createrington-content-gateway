import { createServer, type IncomingMessage, type Server } from "node:http";
import type { AddressInfo } from "node:net";

export interface Seen {
  method: string;
  url: string;
  headers: IncomingMessage["headers"];
  body: string;
}

export interface Reply {
  status: number;
  headers?: Record<string, string>;
  body?: string;
  delayMs?: number;
}

/** A stand-in for the gateway that answers the replies queued for it, in order, and records what it was sent. */
export class FakeGateway {
  readonly seen: Seen[] = [];
  private readonly replies: Reply[] = [];
  private server: Server | undefined;
  private port = 0;

  get baseUrl(): string {
    return `http://127.0.0.1:${this.port}`;
  }

  reply(...replies: Reply[]): void {
    this.replies.push(...replies);
  }

  async start(): Promise<void> {
    const server = createServer((request, response) => {
      const chunks: Buffer[] = [];
      request.on("data", (chunk: Buffer) => chunks.push(chunk));
      request.on("end", () => {
        this.seen.push({
          method: request.method ?? "",
          url: request.url ?? "",
          headers: request.headers,
          body: Buffer.concat(chunks).toString("utf8"),
        });
        const reply = this.replies.shift() ?? { status: 500, body: "" };
        setTimeout(() => {
          response.writeHead(reply.status, reply.headers ?? {});
          response.end(reply.body ?? "");
        }, reply.delayMs ?? 0);
      });
    });
    await new Promise<void>((resolve) =>
      server.listen(0, "127.0.0.1", resolve),
    );
    this.server = server;
    this.port = (server.address() as AddressInfo).port;
  }

  async stop(): Promise<void> {
    const server = this.server;
    if (server === undefined) return;
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
}

export function json(status: number, body: unknown, cache?: string): Reply {
  return {
    status,
    headers: {
      "content-type": "application/json",
      ...(cache === undefined ? {} : { "x-cache": cache }),
    },
    body: JSON.stringify(body),
  };
}
