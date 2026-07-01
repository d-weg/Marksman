// HTTP server that routes incoming requests to their handlers.

export type Handler = (path: string) => string;

/** A minimal HTTP router mapping request paths to handlers. */
export class Server {
  private routes = new Map<string, Handler>();

  /** Register a handler for an HTTP route. */
  route(path: string, handler: Handler): void {
    this.routes.set(path, handler);
  }

  /** Dispatch an incoming request to its route handler. */
  handle(path: string): string {
    const h = this.routes.get(path);
    return h ? h(path) : "404 not found";
  }
}
