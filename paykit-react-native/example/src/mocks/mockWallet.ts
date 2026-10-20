// A mock wallet behind the Paykit payment adapter. It moves no funds: the
// receiving details are fixed fake test strings that no Lightning or Bitcoin
// wallet accepts, and building a payment target only echoes the payload.
//
// Every callback is synchronous, as FfiSdkPaymentAdapter requires (see the
// package README, "Callbacks and threads").
import {
  FfiPaymentPayload,
  FfiPrivateReceivingDetailReservationResponseKind,
  FfiReservationAttribution,
  type FfiPaymentTarget,
  type FfiPrivateReceivingDetail,
  type FfiPrivatePaymentEndpointCandidate,
  type FfiPrivatePaymentEndpointReservationCancellation,
  type FfiPrivatePaymentEndpointSelectionRequest,
  type FfiPrivateReceivingDetailReservationResponse,
  type FfiPublicPaymentEndpointCandidate,
  type FfiPublicPaymentEndpointSelectionRequest,
  type FfiPublicReceivingDetail,
  type FfiSdkPaymentAdapter,
} from '@synonymdev/react-native-paykit';

/** Payment Endpoint Identifiers the mock wallet supports, preferred first. */
export const MOCK_ENDPOINT_IDENTIFIERS = [
  'btc-lightning-bolt11',
  'btc-bitcoin-p2tr',
] as const;

/** Fake receiving details for one wallet. They are not valid on any network. */
export function mockReceivingDetails(
  walletLabel: string
): Array<{ identifier: string; payload: string }> {
  return [
    {
      identifier: 'btc-lightning-bolt11',
      payload: `lntest-paykit-rn-example-fake-invoice-${walletLabel}`,
    },
    {
      identifier: 'btc-bitcoin-p2tr',
      payload: `tbtest-paykit-rn-example-fake-address-${walletLabel}`,
    },
  ];
}

const MOCK_ATTRIBUTION = [['wallet', 'paykit-rn-example-mock']] as const;

export class MockWallet implements FfiSdkPaymentAdapter {
  private nextReservation = 1;
  private readonly openReservations = new Set<string>();
  /** Reservation ids cancelled so far, in call order. */
  readonly cancelledReservations: string[] = [];

  constructor(private readonly walletLabel: string) {}

  private receivingDetails(): Array<
    FfiPublicReceivingDetail & FfiPrivateReceivingDetail
  > {
    return mockReceivingDetails(this.walletLabel).map((detail) => ({
      identifier: detail.identifier,
      payload: new FfiPaymentPayload(detail.payload),
    }));
  }

  currentPublicReceivingDetails(): Array<FfiPublicReceivingDetail> {
    return this.receivingDetails();
  }

  /** The same details for every counterparty. */
  currentPrivateReceivingDetails(): Array<FfiPrivateReceivingDetail> {
    return this.receivingDetails();
  }

  /**
   * Reserve every receiving detail for one counterparty. Reservation ids are a
   * per-wallet counter, so the same call sequence always yields the same ids.
   */
  reservePrivateReceivingDetails(): FfiPrivateReceivingDetailReservationResponse {
    return {
      kind: FfiPrivateReceivingDetailReservationResponseKind.Reservations,
      reservations: this.receivingDetails().map((receivingDetail) => {
        const reservationId = `mock-reservation-${this.walletLabel}-${this.nextReservation++}`;
        this.openReservations.add(reservationId);
        return {
          reservationId,
          receivingDetail,
          expiresAt: undefined,
          attribution: new FfiReservationAttribution(new Map(MOCK_ATTRIBUTION)),
        };
      }),
    };
  }

  /** Cancelling is idempotent: an unknown or already cancelled id is a no-op. */
  cancelPrivateReceivingDetailReservation(
    cancellation: FfiPrivatePaymentEndpointReservationCancellation
  ): void {
    if (this.openReservations.delete(cancellation.reservationId)) {
      this.cancelledReservations.push(cancellation.reservationId);
    }
  }

  selectPublicPaymentEndpointIds(
    request: FfiPublicPaymentEndpointSelectionRequest
  ): Array<string> {
    return preferredCandidateIds(request.candidates);
  }

  buildPublicPaymentTarget(
    endpoint: FfiPublicPaymentEndpointCandidate
  ): FfiPaymentTarget {
    return { payload: new FfiPaymentPayload(endpoint.payload.exportText()) };
  }

  selectPrivatePaymentEndpointIds(
    request: FfiPrivatePaymentEndpointSelectionRequest
  ): Array<string> {
    return preferredCandidateIds(request.candidates);
  }

  buildPrivatePaymentTarget(
    endpoint: FfiPrivatePaymentEndpointCandidate
  ): FfiPaymentTarget {
    return { payload: new FfiPaymentPayload(endpoint.payload.exportText()) };
  }
}

/** Supported candidates in the wallet's preference order. */
function preferredCandidateIds(
  candidates: ReadonlyArray<{ candidateId: string; identifier: string }>
): string[] {
  return MOCK_ENDPOINT_IDENTIFIERS.flatMap((identifier) =>
    candidates
      .filter((candidate) => candidate.identifier === identifier)
      .map((candidate) => candidate.candidateId)
  );
}
