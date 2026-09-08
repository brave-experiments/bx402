/**
 * The MPP payment rail: everything specific to the Machine Payments Protocol
 * lives here.
 *
 * See `x402.ts` for the x402 rail and `dispatch.ts` for the neutral router that
 * classifies each request and delegates to whichever rail it is paying on.
 */

import { Mppx, tempo } from "mppx/server";
import { request } from "undici";
import type { Chain } from "viem";
import { formatUnits } from "viem";
import { createClient, http } from "viem/tempo";
import { tempo as tempoMainnet, tempoModerato } from "viem/tempo/chains";
import type { MppConfig } from "./config.js";
import { ENDPOINTS } from "./endpoints.js";
import { AppError } from "./error.js";

/**
 * MPP carries its credential in the `Authorization` request header. Dispatch
 * keys on presence alone, so any value counts as an attempt on this rail.
 */
const CREDENTIAL_HEADER = "authorization";

/** What this rail calls itself in metrics. */
export const RAIL = "mpp";

/**
 * The realm every challenge carries and every credential echoes back. It names
 * this service, so a credential minted for someone else does not verify here.
 */
const REALM = "bx402";

/** The EVM treasury address that receives MPP payments. */
const PAY_TO_EVM = "0xbd9420A98a7Bd6B89765e5715e169481602D9c3d";

/**
 * pathUSD, the stablecoin every charge is priced in. It is a TIP-20 precompile
 * at a fixed address, the same on mainnet and on the testnet, so there is no
 * per-network token to configure. Naming it explicitly matters: left unset the
 * SDK charges USDC on mainnet.
 */
const PATH_USD = "0x20c0000000000000000000000000000000000000";

/** How many decimals pathUSD carries, the scale every amount is written in. */
const CURRENCY_DECIMALS = 6;

/** How long to wait for the endpoint to report its chain before giving up. */
const CHAIN_QUERY_TIMEOUT_MS = 5_000;

/**
 * The Tempo networks this rail serves, keyed by the chain id an endpoint reports.
 * The definitions come from viem rather than literals here, so they cannot drift
 * from the chains the SDK talks to. Whether each is a testnet is stated rather
 * than read off the chain, so a definition that stops carrying the flag cannot
 * quietly turn a testnet into money. Any other chain is refused.
 */
const NETWORKS = new Map<number, { chain: Chain; testnet: boolean }>([
  [tempoMainnet.id, { chain: tempoMainnet, testnet: false }],
  [tempoModerato.id, { chain: tempoModerato, testnet: true }],
]);

/** Whether the request carries an MPP credential. */
export function hasCredential(headers: Headers): boolean {
  return headers.has(CREDENTIAL_HEADER);
}

/**
 * One endpoint's charge, in the form the SDK takes when a challenge is minted.
 *
 * Every field is stated per challenge rather than left to the method's defaults.
 * A chain id on the method config is ignored and silently falls back to mainnet,
 * which would advertise a chain we are not talking to.
 */
export interface Charge {
  /** Price as a decimal amount of the currency, which `decimals` scales. */
  amount: string;
  chainId: number;
  currency: string;
  decimals: number;
  recipient: string;
}

/**
 * The mppx handler this rail drives, named once so the SDK's generics stay in
 * this module.
 */
type Handler = ReturnType<typeof buildHandler>;

/**
 * The MPP handler and the charge for each paid path, wrapped so the rest of the
 * service names this module's type rather than the SDK's.
 */
export interface Client {
  handler: Handler;
  /**
   * The charge per paid path, built once at startup. A path's challenge
   * advertises its own charge and a credential is verified against that same
   * charge, so no path is payable at another's price.
   */
  charges: Map<string, Charge>;
}

/**
 * Build the MPP client from the rail's settings.
 *
 * The chain is whatever the endpoint reports at startup, never guessed from the
 * URL. An unsupported chain is refused, and a testnet is served only when
 * testnets are allowed. An unreachable endpoint or an unusable `MPP_SECRET_KEY`
 * is a startup misconfiguration.
 */
export async function client(rail: MppConfig, allowTestnet: boolean): Promise<Client> {
  const chainId = await getChainId(rail.rpcUrl);
  const network = NETWORKS.get(chainId);
  if (network === undefined) {
    throw AppError.invalidConfig(`MPP: unsupported Tempo chain ${chainId}`);
  }
  if (network.testnet && !allowTestnet) {
    throw AppError.invalidConfig(
      `MPP_RPC_URL: chain ${chainId} is a testnet; set ALLOW_TESTNET=true to accept it`,
    );
  }
  let handler: Handler;
  try {
    handler = buildHandler(rail, network.chain);
  } catch (err: unknown) {
    throw AppError.invalidConfig(`MPP: ${describe(err)}`);
  }
  return { handler, charges: charges(chainId) };
}

/**
 * The SDK handler, pointed at the configured endpoint. Verification asks for a
 * client per credential, and every credential this service accepts names the one
 * chain the endpoint reported, so the same client answers them all.
 */
function buildHandler(rail: MppConfig, chain: Chain) {
  const rpc = createClient({ chain, transport: http(rail.rpcUrl) });
  return Mppx.create({
    realm: REALM,
    secretKey: rail.secretKey,
    methods: [tempo.charge({ getClient: () => rpc })],
  });
}

/**
 * The charge for every paid path, priced from the catalog.
 *
 * The catalog holds base units, while a charge states a decimal amount that
 * `decimals` scales back to those same base units. The conversion runs through
 * `formatUnits` rather than arithmetic here, since writing the base units
 * straight into `amount` would overcharge by a factor of a million.
 */
function charges(chainId: number): Map<string, Charge> {
  const table = new Map<string, Charge>();
  for (const endpoint of ENDPOINTS) {
    table.set(endpoint.path, {
      amount: formatUnits(BigInt(endpoint.priceBaseUnits), CURRENCY_DECIMALS),
      chainId,
      currency: PATH_USD,
      decimals: CURRENCY_DECIMALS,
      recipient: PAY_TO_EVM,
    });
  }
  return table;
}

/** The chain id the RPC endpoint reports for itself (`eth_chainId`). */
async function getChainId(rpcUrl: string): Promise<number> {
  const invalid = (detail: string) => AppError.invalidConfig(`MPP_RPC_URL: ${detail}`);
  let response: Awaited<ReturnType<typeof request>>;
  try {
    response = await request(rpcUrl, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "eth_chainId", params: [] }),
      headersTimeout: CHAIN_QUERY_TIMEOUT_MS,
      bodyTimeout: CHAIN_QUERY_TIMEOUT_MS,
    });
  } catch (err: unknown) {
    throw invalid(`eth_chainId query failed: ${describe(err)}`);
  }
  if (response.statusCode < 200 || response.statusCode > 299) {
    await response.body.dump();
    throw invalid(`eth_chainId query failed: status ${response.statusCode}`);
  }
  let body: unknown;
  try {
    body = await response.body.json();
  } catch (err: unknown) {
    throw invalid(`eth_chainId response is not JSON: ${describe(err)}`);
  }
  const result = (body as { result?: unknown }).result;
  // Lowercase `0x` hex, the one form the JSON-RPC spec writes quantities in. A
  // JSON-RPC error object carries no string result and lands here too.
  if (typeof result !== "string" || !result.startsWith("0x")) {
    throw invalid(`eth_chainId returned no chain id: ${JSON.stringify(body)}`);
  }
  const chainId = Number.parseInt(result.slice(2), 16);
  if (!Number.isSafeInteger(chainId)) {
    throw invalid(`eth_chainId returned no chain id: ${JSON.stringify(body)}`);
  }
  return chainId;
}

/** The message of a failure, for one log line. */
function describe(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
