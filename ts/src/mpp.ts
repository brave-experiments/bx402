/**
 * The MPP payment rail: everything specific to the Machine Payments Protocol
 * lives here.
 *
 * See `x402.ts` for the x402 rail and `dispatch.ts` for the neutral router that
 * classifies each request and delegates to whichever rail it is paying on.
 */

import { Challenge, Credential } from "mppx";
import { Mppx, tempo } from "mppx/server";
import * as Secp256k1 from "ox/Secp256k1";
import { TxEnvelopeTempo } from "ox/tempo";
import { request } from "undici";
import type { Chain } from "viem";
import { formatUnits } from "viem";
import { createClient, http } from "viem/tempo";
import { tempo as tempoMainnet, tempoModerato } from "viem/tempo/chains";
import type { MppConfig } from "./config.js";
import { ENDPOINTS } from "./endpoints.js";
import { AppError } from "./error.js";
import { log } from "./log.js";

/**
 * MPP carries its credential in the `Authorization` request header. Dispatch
 * keys on presence alone, so any value counts as an attempt on this rail.
 */
const CREDENTIAL_HEADER = "authorization";

/**
 * MPP advertises its challenge in the `WWW-Authenticate` response header, the
 * standard place a scheme states what it wants. The value carries the whole
 * charge, so header-only clients read nothing else.
 */
const CHALLENGE_HEADER = "www-authenticate";

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

/** The leading byte that marks a serialized Tempo transaction. */
const TEMPO_TX_TYPE = "0x76";

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
 * MPP's part of the cold `402`: a fresh `WWW-Authenticate: Payment` challenge
 * carrying the charge a credential must answer. `undefined` if it cannot be
 * built, leaving the `402` advertising x402 alone.
 *
 * Minted per request rather than once at startup, because every challenge is
 * signed under the rail's secret and expires.
 */
export async function challenge(
  client: Client,
  path: string,
): Promise<[string, string] | undefined> {
  // Advertise this endpoint's charge and no other. A client that is offered
  // every price at once could pay the cheapest and call the dearest.
  const charge = client.charges.get(path);
  if (charge === undefined) {
    log.error(`no mpp charge for a paid path: ${path}`);
    return undefined;
  }
  try {
    const minted = await client.handler.challenge.tempo.charge(charge);
    return [CHALLENGE_HEADER, Challenge.serialize(minted)];
  } catch {
    log.error("mpp challenge could not be built");
    return undefined;
  }
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

/**
 * The credential payload that pays here: a signed transaction this service
 * broadcasts while verifying it.
 */
export interface TransactionPayload {
  type: "transaction";
  signature: string;
}

/**
 * The MPP credential carried in the `Authorization` header. `undefined` when the
 * header is absent or is not the `Payment <credential>` form, including when it
 * carries some other scheme entirely.
 */
export function credential(headers: Headers): Credential.Credential | undefined {
  const header = headers.get(CREDENTIAL_HEADER);
  if (header === null) {
    return undefined;
  }
  // A header may carry several schemes at once, so pick ours out rather than
  // assuming it stands alone.
  const scheme = Credential.extractPaymentScheme(header);
  if (scheme === null) {
    return undefined;
  }
  try {
    return Credential.deserialize(scheme);
  } catch {
    return undefined;
  }
}

/**
 * The credential's payload, if it pays with a signed transaction.
 *
 * A hash credential says the client already broadcast the transfer itself,
 * settling before anything was checked, so it does not pay here. Neither does a
 * proof, which authorizes nothing to move.
 */
export function transactionPayload(parsed: Credential.Credential): TransactionPayload | undefined {
  const payload = parsed.payload;
  if (typeof payload !== "object" || payload === null) {
    return undefined;
  }
  const { type, signature } = payload as { type?: unknown; signature?: unknown };
  if (type !== "transaction" || typeof signature !== "string") {
    return undefined;
  }
  return { type, signature };
}

/**
 * The payer, recovered from the signed transaction's own signature: the address
 * the transfer draws from, independent of anything the credential claims.
 *
 * Decoded here rather than asked of the SDK, so the address we screen does not
 * depend on the same code that decides whether the payment is good. Lowercase
 * hex, the screener's canonical form for EVM addresses. `undefined` when the
 * payload carries no decodable signed transaction, which verification would
 * refuse anyway.
 */
export function signerAddress(payload: TransactionPayload): string | undefined {
  // Tempo transactions carry their type byte, which is what says how to read the
  // rest. Anything else is not one, whatever it decodes to.
  if (!payload.signature.startsWith(TEMPO_TX_TYPE)) {
    return undefined;
  }
  try {
    const envelope = TxEnvelopeTempo.deserialize(payload.signature as `0x76${string}`);
    // An unsigned or half-formed signature names no payer, so there is nobody to
    // screen and nothing to recover from.
    const signature = envelope.signature?.signature;
    if (signature === undefined || signature.yParity === undefined) {
      return undefined;
    }
    const recovered = Secp256k1.recoverAddress({
      payload: TxEnvelopeTempo.getSignPayload(envelope),
      signature: { r: signature.r, s: signature.s, yParity: signature.yParity },
    });
    return recovered.toLowerCase();
  } catch {
    return undefined;
  }
}

/** The message of a failure, for one log line. */
function describe(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
